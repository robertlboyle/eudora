// No console window behind release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Eudora on a desktop: the window is the library `eudora_app_lib` (shared with the phone apps).

fn main() {
    eudora_app_lib::run();
}
