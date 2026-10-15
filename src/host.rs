use crate::{Buffer, HostApi};
use serde_json::{json, Value};
use std::{
    ffi::{c_char, c_void, CString},
    ptr,
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Copy)]
struct HostBridge {
    context: usize,
    call: unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut Buffer) -> i32,
    free_buffer: Option<unsafe extern "C" fn(*mut c_void, usize)>,
}

static HOST: OnceLock<Mutex<Option<HostBridge>>> = OnceLock::new();

pub fn install(api: &HostApi) -> Result<(), String> {
    let call = api.call.ok_or_else(|| "宿主未提供 RPC 回调".to_string())?;
    let bridge = HostBridge {
        context: api.host_ctx as usize,
        call,
        free_buffer: api.free_buffer,
    };
    let lock = HOST.get_or_init(|| Mutex::new(None));
    let mut slot = lock.lock().map_err(|_| "宿主 RPC 状态已损坏".to_string())?;
    *slot = Some(bridge);
    Ok(())
}

pub fn clear() {
    if let Some(lock) = HOST.get() {
        if let Ok(mut slot) = lock.lock() {
            *slot = None;
        }
    }
}

pub fn call(method: &str, payload: &Value) -> Result<Value, String> {
    let bridge = {
        let lock = HOST
            .get()
            .ok_or_else(|| "宿主 RPC 尚未初始化".to_string())?;
        let slot = lock.lock().map_err(|_| "宿主 RPC 状态已损坏".to_string())?;
        slot.ok_or_else(|| "宿主 RPC 已关闭".to_string())?
    };

    let method_c = CString::new(method).map_err(|_| "宿主 RPC 方法名无效".to_string())?;
    let request = serde_json::to_vec(payload)
        .map_err(|error| format!("编码宿主 RPC {method} 失败：{error}"))?;
    let mut response = Buffer {
        ptr: ptr::null_mut(),
        len: 0,
    };
    let code = unsafe {
        (bridge.call)(
            bridge.context as *mut c_void,
            method_c.as_ptr(),
            request.as_ptr(),
            request.len(),
            &mut response,
        )
    };

    let raw = if response.ptr.is_null() || response.len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(response.ptr, response.len).to_vec() }
    };
    if !response.ptr.is_null() {
        if let Some(free_buffer) = bridge.free_buffer {
            unsafe { free_buffer(response.ptr as *mut c_void, response.len) };
        }
    }

    if raw.is_empty() {
        return Err(format!("宿主 RPC {method} 返回空响应，code={code}"));
    }
    let envelope: Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("解析宿主 RPC {method} 响应失败：{error}"))?;
    if code != 0 || envelope["ok"].as_bool() != Some(true) {
        let message = envelope["error"]["message"]
            .as_str()
            .unwrap_or("宿主 RPC 调用失败");
        return Err(format!("{message}，code={code}"));
    }
    Ok(envelope.get("result").cloned().unwrap_or_else(|| json!({})))
}

pub fn warn(callback_id: &str, auth_id: &str, message: &str) {
    let _ = call(
        "host.log",
        &json!({
            "host_callback_id": callback_id,
            "level": "warn",
            "message": "cpa-devin 上游模型目录读取失败，已使用内置目录",
            "fields": {
                "auth_id": auth_id,
                "error": message
            }
        }),
    );
}
