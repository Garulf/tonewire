//! Per-app default device switching through the undocumented
//! `IAudioPolicyConfigFactory` (Windows 10 21H2 and later), the interface
//! Windows' own "App volume and device preferences" page uses.

use std::ffi::c_void;

use tonewire_graph::Direction;
use windows::Win32::Media::Audio::{
    EDataFlow, ERole, eCapture, eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoGetActivationFactory, RoInitialize};
use windows::core::{
    HRESULT, HSTRING, IInspectable, IUnknown, IUnknown_Vtbl, Interface, interface,
};

const RENDER_INTERFACE: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const CAPTURE_INTERFACE: &str = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";
const MMDEVAPI: &str = r"\\?\SWD#MMDEVAPI#";

#[interface("ab3d4648-e242-459f-b02f-541c70306324")]
unsafe trait IAudioPolicyConfigFactory: IUnknown {
    fn get_iids(&self) -> HRESULT;
    fn get_runtime_class_name(&self) -> HRESULT;
    fn get_trust_level(&self) -> HRESULT;
    fn add_ctx_volume_change(&self) -> HRESULT;
    fn remove_ctx_volume_changed(&self) -> HRESULT;
    fn add_ringer_vibrate_state_changed(&self) -> HRESULT;
    fn remove_ringer_vibrate_state_change(&self) -> HRESULT;
    fn set_volume_group_gain_for_id(&self) -> HRESULT;
    fn get_volume_group_gain_for_id(&self) -> HRESULT;
    fn get_active_volume_group_for_endpoint_id(&self) -> HRESULT;
    fn get_volume_groups_for_endpoint(&self) -> HRESULT;
    fn get_current_volume_context(&self) -> HRESULT;
    fn set_volume_group_mute_for_id(&self) -> HRESULT;
    fn get_volume_group_mute_for_id(&self) -> HRESULT;
    fn set_ringer_vibrate_state(&self) -> HRESULT;
    fn get_ringer_vibrate_state(&self) -> HRESULT;
    fn set_preferred_chat_application(&self) -> HRESULT;
    fn reset_preferred_chat_application(&self) -> HRESULT;
    fn get_preferred_chat_application(&self) -> HRESULT;
    fn get_current_chat_applications(&self) -> HRESULT;
    fn add_chat_context_changed(&self) -> HRESULT;
    fn remove_chat_context_changed(&self) -> HRESULT;
    fn set_persisted_default_audio_endpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut c_void,
    ) -> HRESULT;
    fn get_persisted_default_audio_endpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut *mut c_void,
    ) -> HRESULT;
    fn clear_all_persisted_application_default_endpoints(&self) -> HRESULT;
}

pub struct PolicyConfig(IAudioPolicyConfigFactory);

// SAFETY: the factory is activated in the multithreaded apartment and is only
// used from the daemon's control thread.
unsafe impl Send for PolicyConfig {}

impl PolicyConfig {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: WinRT activation of the in-box AudioPolicyConfig factory.
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
            let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
            let factory: IInspectable = RoGetActivationFactory(&class)?;
            Ok(Self(factory.cast()?))
        }
    }

    /// Points `pid`'s default playback (or recording) device at `device`, or
    /// back to the system default when `device` is `None`.
    pub fn redirect(
        &self,
        pid: u32,
        direction: Direction,
        device: Option<&str>,
    ) -> windows::core::Result<()> {
        let (flow, suffix) = match direction {
            Direction::Playback => (eRender, RENDER_INTERFACE),
            Direction::Capture => (eCapture, CAPTURE_INTERFACE),
        };
        let path = device.map(|id| HSTRING::from(format!("{MMDEVAPI}{id}{suffix}")));
        for role in [eConsole, eMultimedia, eCommunications] {
            // SAFETY: HSTRING is a transparent wrapper over its handle; `path`
            // outlives the call, and a null handle clears the redirection.
            let raw = path.as_ref().map_or(std::ptr::null_mut(), |h| unsafe {
                std::mem::transmute_copy::<HSTRING, *mut c_void>(h)
            });
            unsafe {
                self.0
                    .set_persisted_default_audio_endpoint(pid, flow, role, raw)
                    .ok()?
            };
        }
        Ok(())
    }
}
