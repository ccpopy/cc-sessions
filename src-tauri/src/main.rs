#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if cc_session_manager_lib::app_update::run_update_helper_from_args() {
        return;
    }
    cc_session_manager_lib::run()
}
