use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};

type Status = c_int;
type Handle = *mut c_void;

const SUCCESS: Status = 0;

#[cfg(windows)]
pub const LIBRARY: &str = "NVAudioEffects.dll";
#[cfg(not(windows))]
pub const LIBRARY: &str = "libnv_audiofx.so";

type GetEffectList = unsafe extern "C" fn(*mut c_int, *mut *const *const c_char) -> Status;
type CreateEffect = unsafe extern "C" fn(*const c_char, *mut Handle) -> Status;
type DestroyEffect = unsafe extern "C" fn(Handle) -> Status;
type SetU32 = unsafe extern "C" fn(Handle, *const c_char, c_uint) -> Status;
type SetString = unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> Status;
type SetFloat = unsafe extern "C" fn(Handle, *const c_char, f32) -> Status;
type GetU32 = unsafe extern "C" fn(Handle, *const c_char, *mut c_uint) -> Status;
type Load = unsafe extern "C" fn(Handle) -> Status;
type Run = unsafe extern "C" fn(Handle, *const *const f32, *mut *mut f32, c_uint, c_uint) -> Status;

/// The NVIDIA Audio Effects SDK, loaded from disk at runtime.
pub struct Api {
    _library: Library,
    root: PathBuf,
    get_effect_list: GetEffectList,
    create_effect: CreateEffect,
    destroy_effect: DestroyEffect,
    set_u32: SetU32,
    set_string: SetString,
    set_float: SetFloat,
    get_u32: GetU32,
    load: Load,
    run: Run,
}

fn status(name: &str, code: Status) -> Result<(), String> {
    if code == SUCCESS {
        Ok(())
    } else {
        Err(format!("{name} failed with NvAFX status {code}"))
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

impl Api {
    pub fn load(path: &Path) -> Result<Self, String> {
        // SAFETY: loading the vendor library runs its initializers; the
        // symbol types below match nvAudioEffects.h.
        unsafe {
            let library =
                Library::new(path).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
            macro_rules! symbol {
                ($name:literal) => {{
                    let s: Symbol<'_, _> = library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("{} is missing {}: {e}", path.display(), $name))?;
                    *s
                }};
            }
            let api = Self {
                get_effect_list: symbol!("NvAFX_GetEffectList"),
                create_effect: symbol!("NvAFX_CreateEffect"),
                destroy_effect: symbol!("NvAFX_DestroyEffect"),
                set_u32: symbol!("NvAFX_SetU32"),
                set_string: symbol!("NvAFX_SetString"),
                set_float: symbol!("NvAFX_SetFloat"),
                get_u32: symbol!("NvAFX_GetU32"),
                load: symbol!("NvAFX_Load"),
                run: symbol!("NvAFX_Run"),
                root: path.parent().map(Path::to_path_buf).unwrap_or_default(),
                _library: library,
            };
            Ok(api)
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn effects(&self) -> Vec<String> {
        let mut count: c_int = 0;
        let mut list: *const *const c_char = std::ptr::null();
        // SAFETY: the SDK fills `count` and a static array of C strings.
        unsafe {
            if (self.get_effect_list)(&mut count, &mut list) != SUCCESS || list.is_null() {
                return Vec::new();
            }
            (0..count.max(0) as usize)
                .filter_map(|i| {
                    let p = *list.add(i);
                    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
                })
                .collect()
        }
    }
}

/// One loaded effect instance. Not `Sync`: each lives on one worker thread.
pub struct Effect<'a> {
    api: &'a Api,
    handle: Handle,
}

// SAFETY: an effect handle is only ever used by one thread at a time.
unsafe impl Send for Effect<'_> {}

impl Drop for Effect<'_> {
    fn drop(&mut self) {
        // SAFETY: handle came from NvAFX_CreateEffect and is destroyed once.
        unsafe { (self.api.destroy_effect)(self.handle) };
    }
}

impl<'a> Effect<'a> {
    pub fn create(api: &'a Api, selector: &str) -> Result<Self, String> {
        let mut handle: Handle = std::ptr::null_mut();
        let name = cstring(selector);
        // SAFETY: out-pointer is valid; selector is NUL terminated.
        status("NvAFX_CreateEffect", unsafe {
            (api.create_effect)(name.as_ptr(), &mut handle)
        })?;
        Ok(Self { api, handle })
    }

    pub fn set_u32(&mut self, param: &str, value: u32) -> Result<(), String> {
        let p = cstring(param);
        // SAFETY: valid handle and NUL-terminated selector.
        status(param, unsafe {
            (self.api.set_u32)(self.handle, p.as_ptr(), value)
        })
    }

    pub fn set_string(&mut self, param: &str, value: &str) -> Result<(), String> {
        let (p, v) = (cstring(param), cstring(value));
        // SAFETY: valid handle and NUL-terminated strings.
        status(param, unsafe {
            (self.api.set_string)(self.handle, p.as_ptr(), v.as_ptr())
        })
    }

    pub fn set_float(&mut self, param: &str, value: f32) -> Result<(), String> {
        let p = cstring(param);
        // SAFETY: valid handle and NUL-terminated selector.
        status(param, unsafe {
            (self.api.set_float)(self.handle, p.as_ptr(), value)
        })
    }

    pub fn get_u32(&self, param: &str) -> Result<u32, String> {
        let p = cstring(param);
        let mut value: c_uint = 0;
        // SAFETY: valid handle, selector and out-pointer.
        status(param, unsafe {
            (self.api.get_u32)(self.handle, p.as_ptr(), &mut value)
        })?;
        Ok(value)
    }

    pub fn load(&mut self) -> Result<(), String> {
        // SAFETY: valid handle.
        status("NvAFX_Load", unsafe { (self.api.load)(self.handle) })
    }

    /// Processes one mono frame of exactly the effect's frame size.
    pub fn run(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), String> {
        let inputs = [input.as_ptr()];
        let mut outputs = [output.as_mut_ptr()];
        // SAFETY: one channel each, `input.len()` samples, buffers outlive the call.
        status("NvAFX_Run", unsafe {
            (self.api.run)(
                self.handle,
                inputs.as_ptr(),
                outputs.as_mut_ptr(),
                input.len() as c_uint,
                1,
            )
        })
    }
}
