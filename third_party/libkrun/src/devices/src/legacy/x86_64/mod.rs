pub mod cmos;
#[cfg(target_os = "windows")]
pub mod i8254;
#[cfg(target_os = "windows")]
pub mod i8259;
pub mod serial;
