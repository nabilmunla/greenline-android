// Prevents an extra console window on Windows release builds. The phone build uses lib.rs `run()`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    app_lib::run()
}
