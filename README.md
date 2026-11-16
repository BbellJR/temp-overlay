# temp-overlay

Overlay เล็ก ๆ ที่ลอยอยู่บนสุดของจอตลอดเวลา แสดงอุณหภูมิ **CPU / GPU / RAM / SSD** (เขียนด้วย Rust + Win32 API ล้วน)

- คลิกทะลุได้ ไม่แย่งโฟกัส ไม่โผล่ใน Alt+Tab หรือ taskbar
- อัปเดตทุก 1 วินาที สีของตัวเลขเปลี่ยนตามความร้อน (เขียว → เหลือง → แดง)

| Hotkey       | ทำอะไร                          |
|--------------|--------------------------------|
| Ctrl+Alt+T   | ซ่อน / แสดง                     |
| Ctrl+Alt+P   | ย้ายไปมุมจอถัดไป (ซ้ายบน → ขวาบน → ขวาล่าง → ซ้ายล่าง) |
| Ctrl+Alt+Q   | ปิดโปรแกรม                       |

## ค่าแต่ละตัวมาจากไหน

| ค่า | แหล่งข้อมูล | ต้องมี LibreHardwareMonitor ไหม |
|-----|-----------|------|
| GPU | NVML (`nvml.dll` มากับไดรเวอร์ NVIDIA) | ไม่ต้อง |
| SSD | `IOCTL_STORAGE_QUERY_PROPERTY` ของ Windows | ไม่ต้อง |
| CPU | LibreHardwareMonitor | **ต้อง** |
| RAM | LibreHardwareMonitor (เซนเซอร์บนแรม DDR5) | **ต้อง** |

Windows ไม่มี API ให้โปรแกรมทั่วไปอ่านอุณหภูมิ CPU และ RAM ได้โดยตรง ต้องใช้ kernel driver
ดังนั้นโปรแกรมนี้จึงดึงค่าจาก web server ในเครื่องของ LibreHardwareMonitor แทน
ถ้าไม่ได้เปิด LibreHardwareMonitor ไว้ ช่อง CPU/RAM จะขึ้น `--` และมีข้อความ `LHM offline`

### ตั้งค่า LibreHardwareMonitor (ทำครั้งเดียว)

1. ติดตั้ง: `winget install LibreHardwareMonitor.LibreHardwareMonitor` (หรือโหลดจาก GitHub)
2. เปิดแบบ **Run as administrator**
3. เมนู **Options → Remote Web Server → Run** (ใช้ port 8085 ตามค่าเริ่มต้น)
4. ถ้าอยากให้เปิดเองตอนเปิดเครื่อง: ติ๊ก **Options → Run On Windows Startup**, **Start Minimized** และ **Minimize To Tray**

ถ้าใช้ port อื่น: `temp-overlay.exe --port 8086`

## Build

```powershell
cargo build --release
# ได้ไฟล์ target\release\temp-overlay.exe
```

ตรวจค่าที่อ่านได้โดยไม่เปิด overlay:

```powershell
.\target\release\temp-overlay.exe --once | more
```

## ให้เปิดเองตอนเปิดเครื่อง

กด `Win+R` → พิมพ์ `shell:startup` → สร้าง shortcut ของ `temp-overlay.exe` ไว้ในโฟลเดอร์นั้น

## ข้อจำกัด

- เกมที่รันแบบ **Exclusive Fullscreen** จะทับ overlay ทุกตัวที่ไม่ได้ inject เข้าเกม
  ให้ตั้งเกมเป็น **Borderless / Windowed Fullscreen** แทน (เกมส่วนใหญ่บน Windows 11 ก็ใช้ได้อยู่แล้ว)
- อุณหภูมิ RAM จะมีก็ต่อเมื่อแรมมีเซนเซอร์ (DDR5 ส่วนใหญ่มี) และ LibreHardwareMonitor อ่าน SMBus ของเครื่องได้
  โน้ตบุ๊กบางรุ่นล็อก SMBus ไว้ ซึ่งในกรณีนั้นจะไม่มีโปรแกรมไหนอ่านได้

## ปรับแต่ง

ค่าต่าง ๆ อยู่ต้นไฟล์ `src/main.rs`: ขนาดฟอนต์ (`FONT_PT`), ความทึบ (`OPACITY`), สี และเกณฑ์ความร้อนของแต่ละตัว (`ROWS`)
