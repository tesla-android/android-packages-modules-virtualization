// Copyright 2021, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of the AIDL interface of the Virt Manager.

use crate::config::VmConfig;
use crate::crosvm::VmInstance;
use crate::{Cid, FIRST_GUEST_CID};
use ::binder::FromIBinder; // TODO(dbrazdil): remove once b/182890877 is fixed
use android_system_virtmanager::aidl::android::system::virtmanager::IVirtManager::IVirtManager;
use android_system_virtmanager::aidl::android::system::virtmanager::IVirtualMachine::{
    BnVirtualMachine, IVirtualMachine,
};
use android_system_virtmanager::aidl::android::system::virtmanager::VirtualMachineDebugInfo::VirtualMachineDebugInfo;
use android_system_virtmanager::binder::{
    self, Interface, ParcelFileDescriptor, StatusCode, Strong, ThreadState,
};
use log::error;
use std::fs::File;
use std::sync::{Arc, Mutex, Weak};

pub const BINDER_SERVICE_IDENTIFIER: &str = "android.system.virtmanager";

// TODO(qwandor): Use PermissionController once it is available to Rust.
/// Only processes running with one of these UIDs are allowed to call debug methods.
const DEBUG_ALLOWED_UIDS: [u32; 2] = [0, 2000];

/// Implementation of `IVirtManager`, the entry point of the AIDL service.
#[derive(Debug, Default)]
pub struct VirtManager {
    state: Mutex<State>,
}

impl Interface for VirtManager {}

impl IVirtManager for VirtManager {
    /// Create and start a new VM with the given configuration, assigning it the next available CID.
    ///
    /// Returns a binder `IVirtualMachine` object referring to it, as a handle for the client.
    fn startVm(
        &self,
        config_path: &str,
        log_fd: Option<&ParcelFileDescriptor>,
    ) -> binder::Result<Strong<dyn IVirtualMachine>> {
        let state = &mut *self.state.lock().unwrap();
        let cid = state.next_cid;
        let log_fd = log_fd
            .map(|fd| fd.as_ref().try_clone().map_err(|_| StatusCode::UNKNOWN_ERROR))
            .transpose()?;
        let instance = Arc::new(start_vm(config_path, cid, log_fd)?);
        // TODO(qwandor): keep track of which CIDs are currently in use so that we can reuse them.
        state.next_cid = state.next_cid.checked_add(1).ok_or(StatusCode::UNKNOWN_ERROR)?;
        state.add_vm(Arc::downgrade(&instance));
        Ok(VirtualMachine::create(instance))
    }

    /// Get a list of all currently running VMs. This method is only intended for debug purposes,
    /// and as such is only permitted from the shell user.
    fn debugListVms(&self) -> binder::Result<Vec<VirtualMachineDebugInfo>> {
        if !debug_access_allowed() {
            return Err(StatusCode::PERMISSION_DENIED.into());
        }

        let state = &mut *self.state.lock().unwrap();
        let vms = state.vms();
        let cids = vms
            .into_iter()
            .map(|vm| VirtualMachineDebugInfo {
                cid: vm.cid as i32,
                configPath: vm.config_path.clone(),
            })
            .collect();
        Ok(cids)
    }

    /// Hold a strong reference to a VM in Virt Manager. This method is only intended for debug
    /// purposes, and as such is only permitted from the shell user.
    fn debugHoldVmRef(&self, vmref: &dyn IVirtualMachine) -> binder::Result<()> {
        if !debug_access_allowed() {
            return Err(StatusCode::PERMISSION_DENIED.into());
        }

        // Workaround for b/182890877.
        let vm: Strong<dyn IVirtualMachine> = FromIBinder::try_from(vmref.as_binder()).unwrap();

        let state = &mut *self.state.lock().unwrap();
        state.debug_hold_vm(vm);
        Ok(())
    }

    /// Drop reference to a VM that is being held by Virt Manager. Returns the reference if VM was
    /// found and None otherwise. This method is only intended for debug purposes, and as such is
    /// only permitted from the shell user.
    fn debugDropVmRef(&self, cid: i32) -> binder::Result<Option<Strong<dyn IVirtualMachine>>> {
        if !debug_access_allowed() {
            return Err(StatusCode::PERMISSION_DENIED.into());
        }

        let state = &mut *self.state.lock().unwrap();
        Ok(state.debug_drop_vm(cid))
    }
}

/// Check whether the caller of the current Binder method is allowed to call debug methods.
fn debug_access_allowed() -> bool {
    let uid = ThreadState::get_calling_uid();
    log::trace!("Debug method call from UID {}.", uid);
    DEBUG_ALLOWED_UIDS.contains(&uid)
}

/// Implementation of the AIDL `IVirtualMachine` interface. Used as a handle to a VM.
#[derive(Debug)]
struct VirtualMachine {
    instance: Arc<VmInstance>,
}

impl VirtualMachine {
    fn create(instance: Arc<VmInstance>) -> Strong<dyn IVirtualMachine> {
        let binder = VirtualMachine { instance };
        BnVirtualMachine::new_binder(binder)
    }
}

impl Interface for VirtualMachine {}

impl IVirtualMachine for VirtualMachine {
    fn getCid(&self) -> binder::Result<i32> {
        Ok(self.instance.cid as i32)
    }
}

/// The mutable state of the Virt Manager. There should only be one instance of this struct.
#[derive(Debug)]
struct State {
    /// The next available unused CID.
    next_cid: Cid,

    /// The VMs which have been started. When VMs are started a weak reference is added to this list
    /// while a strong reference is returned to the caller over Binder. Once all copies of the
    /// Binder client are dropped the weak reference here will become invalid, and will be removed
    /// from the list opportunistically the next time `add_vm` is called.
    vms: Vec<Weak<VmInstance>>,

    /// Vector of strong VM references held on behalf of users that cannot hold them themselves.
    /// This is only used for debugging purposes.
    debug_held_vms: Vec<Strong<dyn IVirtualMachine>>,
}

impl State {
    /// Get a list of VMs which are currently running.
    fn vms(&self) -> Vec<Arc<VmInstance>> {
        // Attempt to upgrade the weak pointers to strong pointers.
        self.vms.iter().filter_map(Weak::upgrade).collect()
    }

    /// Add a new VM to the list.
    fn add_vm(&mut self, vm: Weak<VmInstance>) {
        // Garbage collect any entries from the stored list which no longer exist.
        self.vms.retain(|vm| vm.strong_count() > 0);

        // Actually add the new VM.
        self.vms.push(vm);
    }

    /// Store a strong VM reference.
    fn debug_hold_vm(&mut self, vm: Strong<dyn IVirtualMachine>) {
        self.debug_held_vms.push(vm);
    }

    /// Retrieve and remove a strong VM reference.
    fn debug_drop_vm(&mut self, cid: i32) -> Option<Strong<dyn IVirtualMachine>> {
        let pos = self.debug_held_vms.iter().position(|vm| vm.getCid() == Ok(cid))?;
        Some(self.debug_held_vms.swap_remove(pos))
    }
}

impl Default for State {
    fn default() -> Self {
        State { next_cid: FIRST_GUEST_CID, vms: vec![], debug_held_vms: vec![] }
    }
}

/// Start a new VM instance from the given VM config filename. This assumes the VM is not already
/// running.
fn start_vm(config_path: &str, cid: Cid, log_fd: Option<File>) -> binder::Result<VmInstance> {
    let config = VmConfig::load(config_path).map_err(|e| {
        error!("Failed to load VM config {}: {:?}", config_path, e);
        StatusCode::BAD_VALUE
    })?;
    Ok(VmInstance::start(&config, cid, config_path, log_fd).map_err(|e| {
        error!("Failed to start VM {}: {:?}", config_path, e);
        StatusCode::UNKNOWN_ERROR
    })?)
}