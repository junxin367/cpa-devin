mod devin;
mod host;

use serde_json::{json, Value};
use std::{
    ffi::{c_char, c_void, CStr},
    ptr,
};

const ABI_VERSION: u32 = 1;
const SCHEMA_VERSION: u32 = 6;

#[repr(C)]
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
}

#[repr(C)]
pub struct HostApi {
    abi_version: u32,
    host_ctx: *mut c_void,
    call: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut Buffer) -> i32,
    >,
    free_buffer: Option<unsafe extern "C" fn(*mut c_void, usize)>,
}

#[repr(C)]
pub struct PluginApi {
    abi_version: u32,
    call: Option<unsafe extern "C" fn(*const c_char, *const u8, usize, *mut Buffer) -> i32>,
    free_buffer: Option<unsafe extern "C" fn(*mut c_void, usize)>,
    shutdown: Option<unsafe extern "C" fn()>,
}

#[no_mangle]
pub unsafe extern "C" fn cliproxy_plugin_init(
    host_api: *const HostApi,
    plugin_api: *mut PluginApi,
) -> i32 {
    if host_api.is_null()
        || plugin_api.is_null()
        || (*host_api).abi_version != ABI_VERSION
        || (*host_api).call.is_none()
    {
        return 1;
    }
    if host::install(&*host_api).is_err() {
        return 1;
    }
    *plugin_api = PluginApi {
        abi_version: ABI_VERSION,
        call: Some(call),
        free_buffer: Some(free),
        shutdown: Some(shutdown),
    };
    0
}

unsafe extern "C" fn call(
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut Buffer,
) -> i32 {
    if response.is_null() {
        return 1;
    }
    *response = Buffer {
        ptr: ptr::null_mut(),
        len: 0,
    };
    if method.is_null() || (request.is_null() && request_len > 0) {
        return 1;
    }

    let method = match CStr::from_ptr(method).to_str() {
        Ok(value) => value,
        Err(_) => return 1,
    };
    let request_bytes = if request_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(request, request_len)
    };
    let result = std::panic::catch_unwind(|| {
        let request_json = if request_bytes.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice(request_bytes) {
                Ok(value) => value,
                Err(error) => {
                    return error_envelope(
                        "invalid_request",
                        format!("宿主 RPC JSON 无效：{error}"),
                    )
                }
            }
        };
        dispatch(method, &request_json)
    })
    .unwrap_or_else(|_| error_envelope("plugin_panic", "插件内部异常".to_string()));

    let bytes = result.to_string().into_bytes().into_boxed_slice();
    (*response).len = bytes.len();
    (*response).ptr = Box::into_raw(bytes) as *mut u8;
    0
}

unsafe extern "C" fn free(pointer: *mut c_void, len: usize) {
    if !pointer.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            pointer as *mut u8,
            len,
        )));
    }
}

unsafe extern "C" fn shutdown() {
    host::clear();
}

fn dispatch(method: &str, request: &Value) -> Value {
    match method {
        "plugin.register" => ok(registration()),
        "plugin.reconfigure" => {
            devin::clear_routes();
            ok(registration())
        }
        "plugin.quiesce" => ok(json!({})),
        "plugin.shutdown" => {
            devin::clear_routes();
            ok(json!({}))
        }
        "executor.identifier" => ok(json!({"identifier": "devin"})),
        "model.static" => ok(json!({"Provider": "devin", "Models": []})),
        "model.for_auth" => ok(devin::models_for_auth(request)),
        "model.route" => ok(devin::route_model(request)),
        "response.intercept_after" => ok(devin::intercept_response(request)),
        _ => error_envelope("unknown_method", format!("不支持的宿主调用方法：{method}")),
    }
}

fn registration() -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "metadata": {
            "Name": "Devin 动态模型目录",
            "Version": env!("CARGO_PKG_VERSION"),
            "Author": "junxin367",
            "GitHubRepository": option_env!("CPA_PLUGIN_REPOSITORY")
                .unwrap_or(env!("CARGO_PKG_REPOSITORY")),
            "ConfigFields": []
        },
        "capabilities": {
            "model_provider": true,
            "model_router": true,
            "response_interceptor": true,
            "executor": true,
            "executor_model_scope": "oauth",
            "executor_input_formats": ["openai"],
            "executor_output_formats": ["openai"]
        }
    })
}

fn ok(result: Value) -> Value {
    json!({"ok": true, "result": result})
}

fn error_envelope(code: &str, message: String) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message
        }
    })
}
