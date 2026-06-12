#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    {{APP_CRATE}}_lib::run()
}
