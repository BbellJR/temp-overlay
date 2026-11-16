//! Temperature sources.
//!
//! - GPU (NVIDIA): read directly through NVML (nvml.dll ships with the driver, loaded at runtime).
//! - SSD (NVMe/SATA): read directly through IOCTL_STORAGE_QUERY_PROPERTY, no admin needed.
//! - CPU and RAM: Windows has no user-mode API for these (they need a kernel driver),
//!   so they come from LibreHardwareMonitor's "Remote Web Server" (http://localhost:8085/data.json).
//!   LibreHardwareMonitor is also used as a fallback for GPU/SSD if the native read fails.

use std::time::Duration;

use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Readings {
    pub cpu: Option<f32>,
    pub gpu: Option<f32>,
    pub ram: Option<f32>,
    pub ssd: Option<f32>,
    /// true when LibreHardwareMonitor's web server answered.
    pub lhm_online: bool,
}

impl Readings {
    pub const EMPTY: Readings = Readings {
        cpu: None,
        gpu: None,
        ram: None,
        ssd: None,
        lhm_online: false,
    };
}

pub struct Sensors {
    nvml: Option<Nvml>,
    agent: ureq::Agent,
    lhm_url: String,
}

impl Sensors {
    pub fn new(lhm_port: u16) -> Self {
        Sensors {
            nvml: Nvml::load(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_millis(800))
                .build(),
            lhm_url: format!("http://127.0.0.1:{lhm_port}/data.json"),
        }
    }

    pub fn read(&mut self) -> Readings {
        let mut r = Readings {
            gpu: self.gpu_nvml(),
            ssd: ssd_native(),
            ..Readings::EMPTY
        };
        if let Some(l) = self.read_lhm() {
            r.lhm_online = true;
            r.cpu = l.cpu;
            r.ram = l.ram;
            r.gpu = r.gpu.or(l.gpu);
            r.ssd = r.ssd.or(l.ssd);
        }
        r
    }

    fn gpu_nvml(&self) -> Option<f32> {
        self.nvml.as_ref()?.max_temp()
    }

    fn read_lhm(&self) -> Option<Readings> {
        let body = self.agent.get(&self.lhm_url).call().ok()?.into_string().ok()?;
        let json: Value = serde_json::from_str(&body).ok()?;
        Some(parse_lhm(&json))
    }
}

// ---------------------------------------------------------------------------
// NVIDIA GPU via nvml.dll (loaded dynamically so the program still runs on non-NVIDIA PCs)
// ---------------------------------------------------------------------------

type NvmlDevice = *mut std::ffi::c_void;

struct Nvml {
    get_count: unsafe extern "C" fn(*mut u32) -> i32,
    get_handle: unsafe extern "C" fn(u32, *mut NvmlDevice) -> i32,
    get_temp: unsafe extern "C" fn(NvmlDevice, i32, *mut u32) -> i32,
}

impl Nvml {
    fn load() -> Option<Nvml> {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        let candidates = [
            "nvml.dll",
            r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll",
        ];
        unsafe {
            let lib = candidates.iter().find_map(|p| {
                let w: Vec<u16> = p.encode_utf16().chain(Some(0)).collect();
                let h = LoadLibraryW(w.as_ptr());
                (!h.is_null()).then_some(h)
            })?;
            let sym = |name: &[u8]| GetProcAddress(lib, name.as_ptr());
            let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(sym(b"nvmlInit_v2\0")?);
            let nvml = Nvml {
                get_count: std::mem::transmute(sym(b"nvmlDeviceGetCount_v2\0")?),
                get_handle: std::mem::transmute(sym(b"nvmlDeviceGetHandleByIndex_v2\0")?),
                get_temp: std::mem::transmute(sym(b"nvmlDeviceGetTemperature\0")?),
            };
            (init() == 0).then_some(nvml)
        }
    }

    fn max_temp(&self) -> Option<f32> {
        const NVML_TEMPERATURE_GPU: i32 = 0;
        unsafe {
            let mut count = 0u32;
            if (self.get_count)(&mut count) != 0 {
                return None;
            }
            (0..count)
                .filter_map(|i| {
                    let mut dev: NvmlDevice = std::ptr::null_mut();
                    let mut t = 0u32;
                    ((self.get_handle)(i, &mut dev) == 0
                        && (self.get_temp)(dev, NVML_TEMPERATURE_GPU, &mut t) == 0)
                        .then_some(t as f32)
                })
                .reduce(f32::max)
        }
    }
}

// ---------------------------------------------------------------------------
// LibreHardwareMonitor data.json parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Cpu,
    Gpu { integrated: bool },
    Ram,
    Ssd,
}

struct Candidate {
    kind: Kind,
    name: String,
    celsius: f32,
}

/// Hardware nodes in data.json are identified by their icon.
fn kind_from_image(image: &str) -> Option<Kind> {
    match image.rsplit('/').next().unwrap_or("") {
        "cpu.png" => Some(Kind::Cpu),
        "nvidia.png" | "ati.png" | "amd.png" => Some(Kind::Gpu { integrated: false }),
        "intel.png" => Some(Kind::Gpu { integrated: true }),
        "ram.png" => Some(Kind::Ram),
        "hdd.png" => Some(Kind::Ssd),
        _ => None,
    }
}

/// "45.0 °C", "45,0 °C" or "113.0 °F" -> Celsius.
fn parse_temp(s: &str) -> Option<f32> {
    let num: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-'))
        .map(|c| if c == ',' { '.' } else { c })
        .collect();
    let v: f32 = num.parse().ok()?;
    let c = if s.contains("°F") { (v - 32.0) * 5.0 / 9.0 } else { v };
    (c > 0.0 && c < 150.0).then_some(c)
}

/// LHM lists configuration values next to real readings under "Temperatures"
/// (e.g. DIMM "Thermal Sensor Critical High Limit = 85 °C", NVMe "Warning Temperature",
/// CPU "Distance to TjMax"). These are not measurements and must be ignored.
fn is_threshold(name: &str) -> bool {
    ["Limit", "Resolution", "Distance", "Warning", "Critical", "TjMax"]
        .iter()
        .any(|k| name.contains(k))
}

fn walk(node: &Value, kind: Option<Kind>, in_temps: bool, out: &mut Vec<Candidate>) {
    let text = node["Text"].as_str().unwrap_or("");
    let image = node["ImageURL"].as_str().unwrap_or("");
    let kind = kind_from_image(image).or(kind);
    let in_temps = in_temps || text == "Temperatures" || image.ends_with("temperature.png");

    let children = node["Children"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if children.is_empty() {
        let is_temp = (in_temps || node["Type"].as_str() == Some("Temperature")) && !is_threshold(text);
        if let (true, Some(kind), Some(c)) = (is_temp, kind, node["Value"].as_str().and_then(parse_temp)) {
            out.push(Candidate { kind, name: text.to_string(), celsius: c });
        }
        return;
    }
    for child in children {
        walk(child, kind, in_temps, out);
    }
}

/// Max over sensors whose name is in `preferred` (first name that matches anything wins),
/// otherwise max over every sensor of that kind.
fn pick(cands: &[&Candidate], preferred: &[&str]) -> Option<f32> {
    for p in preferred {
        let v = cands.iter().filter(|c| c.name == *p).map(|c| c.celsius).reduce(f32::max);
        if v.is_some() {
            return v;
        }
    }
    cands.iter().map(|c| c.celsius).reduce(f32::max)
}

fn parse_lhm(root: &Value) -> Readings {
    let mut all = Vec::new();
    walk(root, None, false, &mut all);
    let of = |pred: &dyn Fn(Kind) -> bool| all.iter().filter(|c| pred(c.kind)).collect::<Vec<_>>();

    let cpu = of(&|k| k == Kind::Cpu);
    let dgpu = of(&|k| k == Kind::Gpu { integrated: false });
    let igpu = of(&|k| k == Kind::Gpu { integrated: true });
    let ram = of(&|k| k == Kind::Ram);
    let ssd = of(&|k| k == Kind::Ssd);

    Readings {
        cpu: pick(&cpu, &["CPU Package", "Core (Tctl/Tdie)", "Core Max", "Core Average", "CPU Die"]),
        gpu: pick(&dgpu, &["GPU Core"]).or_else(|| pick(&igpu, &["GPU Core"])),
        ram: pick(&ram, &[]),
        ssd: pick(&ssd, &["Composite Temperature", "Temperature"]),
        lhm_online: true,
    }
}

// ---------------------------------------------------------------------------
// Native SSD temperature (IOCTL_STORAGE_QUERY_PROPERTY / StorageDeviceTemperatureProperty)
// ---------------------------------------------------------------------------

fn ssd_native() -> Option<f32> {
    (0..8).filter_map(drive_temp).reduce(f32::max)
}

fn drive_temp(index: u32) -> Option<f32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceTemperatureProperty, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let path: Vec<u16> = format!(r"\\.\PhysicalDrive{index}")
        .encode_utf16()
        .chain(Some(0))
        .collect();

    unsafe {
        // Desired access 0 = query-only handle; works without administrator rights.
        let h = CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return None;
        }

        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceTemperatureProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut buf = [0u8; 512];
        let mut returned = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const _,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        );
        CloseHandle(h);

        // STORAGE_TEMPERATURE_DATA_DESCRIPTOR: InfoCount @12 (u16),
        // TemperatureInfo[0] @24 { Index: u16, Temperature: i16 @26, ... }
        if ok == 0 || returned < 28 {
            return None;
        }
        let count = u16::from_le_bytes([buf[12], buf[13]]);
        if count == 0 {
            return None;
        }
        let t = i16::from_le_bytes([buf[26], buf[27]]) as f32;
        (t > 0.0 && t < 150.0).then_some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values() {
        assert_eq!(parse_temp("45.0 °C"), Some(45.0));
        assert_eq!(parse_temp("45,5 °C"), Some(45.5));
        assert_eq!(parse_temp("212.0 °F"), Some(100.0));
        assert_eq!(parse_temp(""), None);
    }

    #[test]
    fn parses_lhm_tree() {
        let json: Value = serde_json::from_str(
            r#"{"Text":"Sensor","ImageURL":"","Children":[{"Text":"PC","ImageURL":"images_icon/computer.png","Children":[
              {"Text":"13th Gen Intel Core i5-13420H","ImageURL":"images_icon/cpu.png","Children":[
                {"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Children":[
                  {"Text":"CPU Core #1","Value":"60.0 °C","Children":[]},
                  {"Text":"CPU Package","Value":"66.0 °C","Children":[]}]},
                {"Text":"Clocks","ImageURL":"images_icon/clock.png","Children":[
                  {"Text":"Bus Speed","Value":"100.0 MHz","Children":[]}]}]},
              {"Text":"Intel UHD Graphics","ImageURL":"images_icon/intel.png","Children":[]},
              {"Text":"NVIDIA GeForce RTX 3050","ImageURL":"images_icon/nvidia.png","Children":[
                {"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Children":[
                  {"Text":"GPU Core","Value":"51.0 °C","Children":[]},
                  {"Text":"GPU Hot Spot","Value":"60.0 °C","Children":[]}]}]},
              {"Text":"DIMM #1","ImageURL":"images_icon/ram.png","Children":[
                {"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Children":[
                  {"Text":"Temperature","Value":"41.5 °C","Children":[]}]}]},
              {"Text":"DIMM #2","ImageURL":"images_icon/ram.png","Children":[
                {"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Children":[
                  {"Text":"DIMM #2","Value":"43.0 °C","Children":[]},
                  {"Text":"Temperature Sensor Resolution","Value":"0.3 °C","Children":[]},
                  {"Text":"Thermal Sensor High Limit","Value":"55.0 °C","Children":[]},
                  {"Text":"Thermal Sensor Critical High Limit","Value":"85.0 °C","Children":[]}]}]},
              {"Text":"Total Memory","ImageURL":"images_icon/ram.png","Children":[
                {"Text":"Load","ImageURL":"images_icon/load.png","Children":[
                  {"Text":"Memory","Value":"82.1 %","Children":[]}]}]},
              {"Text":"NVMe AirDisk 512GB SSD","ImageURL":"images_icon/hdd.png","Children":[
                {"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Children":[
                  {"Text":"Composite Temperature","Value":"38.0 °C","Children":[]},
                  {"Text":"Temperature #2","Value":"45.0 °C","Children":[]}]}]}
            ]}]}"#,
        )
        .unwrap();
        let r = parse_lhm(&json);
        assert_eq!(r.cpu, Some(66.0));
        assert_eq!(r.gpu, Some(51.0));
        assert_eq!(r.ram, Some(43.0));
        assert_eq!(r.ssd, Some(38.0));
    }
}
