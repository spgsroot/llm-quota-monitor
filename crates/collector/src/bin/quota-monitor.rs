#![windows_subsystem = "windows"]

fn main() {
    if let Err(error) = quota_collector::desktop::run() {
        let title: Vec<u16> = "LLM Quota Monitor".encode_utf16().chain(Some(0)).collect();
        let message: Vec<u16> = error.to_string().encode_utf16().chain(Some(0)).collect();
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                    | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}
