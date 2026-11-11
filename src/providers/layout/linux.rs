use dbus::{blocking::Connection, message::MatchRule};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use std::{ffi, mem, ptr};
use tokio::sync::broadcast;
use x11::xlib::{_XDisplay, _XkbDesc, _XkbStateRec, XGetAtomName, XOpenDisplay, XkbAllocKeyboard, XkbGetNames, XkbGetState};

use crate::config::get_config;
use crate::data_type::DataType;

use super::super::_base::Provider;

fn get_symbols(display: *mut _XDisplay, keyboard: *mut _XkbDesc) -> String {
    unsafe { XkbGetNames(display, 1 << 2, keyboard) };
    let symbols_atom = unsafe { keyboard.read().names.read().symbols };
    let symbols_ptr = unsafe { XGetAtomName(display, symbols_atom) };
    let symbols_cstr = unsafe { ffi::CStr::from_ptr(symbols_ptr) };
    let symbols = String::from_utf8(symbols_cstr.to_bytes().to_vec()).unwrap_or_default();

    tracing::info!("layout symbols: {}", symbols);

    return symbols;
}

fn get_layout_index(display: *mut _XDisplay) -> usize {
    let mut state = unsafe { mem::zeroed::<_XkbStateRec>() };
    unsafe { XkbGetState(display, 0x0100, &mut state) };
    return state.group as usize;
}

fn send_data(value: &str, layouts: &[String], data_sender: &broadcast::Sender<Vec<u8>>) {
    tracing::info!("new layout: '{0}', layout list: {1:?}", value, layouts);
    let index = layouts.into_iter().position(|r| r == value);
    if let Some(index) = index {
        let data = vec![DataType::Layout as u8, index as u8];
        if let Err(error) = data_sender.send(data) {
            tracing::warn!("Layout Provider failed to send data: {}", error);
        }
    }
}

fn send_kde_layout(connection: &Connection, layouts: &[String], data_sender: &broadcast::Sender<Vec<u8>>) -> Result<(), dbus::Error> {
    let proxy = connection.with_proxy("org.kde.keyboard", "/Layouts", Duration::from_secs(1));
    let (index,): (u32,) = proxy.method_call("org.kde.KeyboardLayouts", "getLayout", ())?;
    let (system_layouts,): (Vec<(String, String, String)>,) = proxy.method_call("org.kde.KeyboardLayouts", "getLayoutsList", ())?;
    if let Some((name, _, _)) = system_layouts.get(index as usize) {
        send_data(name, layouts, data_sender);
    }
    Ok(())
}

fn run_kde_layout(layouts: &[String], data_sender: &broadcast::Sender<Vec<u8>>, is_started: &AtomicBool) -> Result<(), dbus::Error> {
    let connection = Connection::new_session()?;
    let mut rule = MatchRule::new_signal("org.kde.KeyboardLayouts", "layoutChanged");
    rule.member = None; // Also refresh after layoutListChanged.
    rule.path = Some("/Layouts".into());
    rule.sender = Some("org.kde.keyboard".into());
    let sender = data_sender.clone();
    let layout_names = layouts.to_vec();
    connection.add_match::<(), _>(rule, move |(), connection, _| {
        if let Err(error) = send_kde_layout(connection, &layout_names, &sender) {
            tracing::warn!("KDE layout update failed: {}", error);
        }
        true
    })?;
    send_kde_layout(&connection, layouts, data_sender)?;
    while is_started.load(Relaxed) {
        connection.process(Duration::from_millis(100))?;
    }
    Ok(())
}

pub struct LayoutProvider {
    data_sender: broadcast::Sender<Vec<u8>>,
    is_started: Arc<AtomicBool>,
}

impl LayoutProvider {
    pub fn new(data_sender: broadcast::Sender<Vec<u8>>) -> Box<dyn Provider> {
        let provider = LayoutProvider {
            data_sender,
            is_started: Arc::new(AtomicBool::new(false)),
        };
        return Box::new(provider);
    }
}

impl Provider for LayoutProvider {
    fn start(&self) {
        tracing::info!("Layout Provider started");
        self.is_started.store(true, Relaxed);
        let layouts = &get_config().layouts;
        let data_sender = self.data_sender.clone();
        let is_started = self.is_started.clone();
        std::thread::spawn(move || {
            if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland") {
                if let Err(error) = run_kde_layout(layouts, &data_sender, &is_started) {
                    tracing::warn!("KDE Wayland layout provider unavailable: {}", error);
                }
                tracing::info!("Layout Provider stopped");
                return;
            }

            let mut synced_layout = usize::MAX;
            let display = unsafe { XOpenDisplay(ptr::null()) };
            if display.is_null() {
                tracing::warn!("X11 layout provider could not open the display");
                return;
            }
            let keyboard = unsafe { XkbAllocKeyboard() };
            let symbols = get_symbols(display, keyboard);
            let symbol_list = symbols.split('+').map(|x| x.to_string()).collect::<Vec<String>>();

            loop {
                if !is_started.load(Relaxed) {
                    break;
                }

                let layout = get_layout_index(display);
                if synced_layout != layout {
                    synced_layout = layout;
                    let layout_symbol = symbol_list.get(layout + 1).map(|x| x.to_string()).unwrap_or_default();
                    let layout_name = layout_symbol.split([':', '(']).next().unwrap_or_default().to_string();
                    send_data(&layout_name, layouts, &data_sender);
                }

                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            tracing::info!("Layout Provider stopped");
        });
    }

    fn stop(&self) {
        self.is_started.store(false, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::blocking::Connection;
    use std::time::Duration;

    struct RestoreLayout(Connection, u32);

    impl Drop for RestoreLayout {
        fn drop(&mut self) {
            let proxy = self.0.with_proxy("org.kde.keyboard", "/Layouts", Duration::from_secs(1));
            let _: Result<(bool,), _> = proxy.method_call("org.kde.KeyboardLayouts", "setLayout", (self.1,));
        }
    }

    struct RunningProvider(Box<dyn Provider>);

    impl Drop for RunningProvider {
        fn drop(&mut self) {
            self.0.stop();
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    #[tokio::test]
    #[ignore = "requires a running KDE desktop; briefly switches and restores its layouts"]
    async fn kde_layout_reports_match_firmware_order() {
        let connection = Connection::new_session().unwrap();
        let proxy = connection.with_proxy("org.kde.keyboard", "/Layouts", Duration::from_secs(1));
        let (original,): (u32,) = proxy.method_call("org.kde.KeyboardLayouts", "getLayout", ()).unwrap();
        let (system_layouts,): (Vec<(String, String, String)>,) =
            proxy.method_call("org.kde.KeyboardLayouts", "getLayoutsList", ()).unwrap();
        let restore = RestoreLayout(connection, original);
        let firmware_order = vec!["us", "ru", "de", "ua"];
        assert!(system_layouts.iter().all(|layout| firmware_order.contains(&layout.0.as_str())));

        let config_path = std::env::temp_dir().join(format!("qmk-hid-layout-test-{}.json", std::process::id()));
        std::fs::write(
            &config_path,
            serde_json::json!({"devices": [], "layouts": firmware_order}).to_string(),
        )
        .unwrap();
        crate::config::load_config(config_path.clone());
        std::fs::remove_file(config_path).unwrap();

        let (sender, mut receiver) = broadcast::channel(16);
        let provider = RunningProvider(LayoutProvider::new(sender));
        provider.0.start();
        let current_name = &system_layouts[original as usize].0;
        let current_index = firmware_order.iter().position(|layout| *layout == current_name).unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("provider must send the active KDE layout on startup")
            .unwrap();
        assert_eq!(first, vec![DataType::Layout as u8, current_index as u8]);

        let proxy = restore.0.with_proxy("org.kde.keyboard", "/Layouts", Duration::from_secs(1));
        for offset in 1..=system_layouts.len() {
            let index = (original as usize + offset) % system_layouts.len();
            let (changed,): (bool,) = proxy.method_call("org.kde.KeyboardLayouts", "setLayout", (index as u32,)).unwrap();
            assert!(changed);
            let name = &system_layouts[index].0;
            let expected = firmware_order.iter().position(|layout| *layout == name).unwrap();
            let report = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report, vec![DataType::Layout as u8, expected as u8]);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(250), receiver.recv()).await.is_err(),
            "idle layout must not produce reports"
        );
    }
}
