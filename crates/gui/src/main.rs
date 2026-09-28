mod server;

use glib::object::ObjectExt;
use gtk4::prelude::*;
use gtk4::gio;
use gtk4::{
    Align, Box, Button, ColumnView, ColumnViewColumn, Entry, FileChooserAction, FileChooserNative,
    HeaderBar, Label, Orientation, ProgressBar, ResponseType, ScrolledWindow,
    SignalListItemFactory, SingleSelection,
};
use libadwaita::{Application, ApplicationWindow, ColorScheme, StyleManager};
use oxidl_core::{suggest_filename, validate_url, DownloadError, DownloadState, DownloadTask, ProgressEvent};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DownloadRecord {
    pub filename: String,
    pub size: String,
    pub status: String,
    pub progress: f64,
    pub speed: String,
    pub url: String,
    pub output_path: String,
}

enum UiMessage {
    Progress(ProgressEvent),
    Error(String),
    Completed,
    Paused,
}

mod download_object {
    use gtk4::glib;
    use gtk4::subclass::prelude::*;

    glib::wrapper! {
        pub struct DownloadObject(ObjectSubclass<imp::DownloadObject>);
    }

    mod imp {
        use gtk4::glib::prelude::*;
        use gtk4::glib::{self, Properties};
        use gtk4::subclass::prelude::*;
        use std::cell::{Cell, RefCell};
        use tokio_util::sync::CancellationToken;

        #[derive(Default, Properties)]
        #[properties(wrapper_type = super::DownloadObject)]
        pub struct DownloadObject {
            #[property(get, set)]
            pub filename: RefCell<String>,
            #[property(get, set)]
            pub size: RefCell<String>,
            #[property(get, set)]
            pub status: RefCell<String>,
            #[property(get, set)]
            pub progress: RefCell<f64>,
            #[property(get, set)]
            pub speed: RefCell<String>,
            #[property(get, set)]
            pub url: RefCell<String>,
            #[property(get, set)]
            pub output_path: RefCell<String>,
            pub cancel_token: RefCell<Option<CancellationToken>>,
            pub removed: Cell<bool>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for DownloadObject {
            const NAME: &'static str = "OxidlDownloadObject";
            type Type = super::DownloadObject;
        }

        impl ObjectImpl for DownloadObject {
            fn properties() -> &'static [glib::ParamSpec] {
                Self::derived_properties()
            }

            fn set_property(&self, id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
                Self::derived_set_property(self, id, value, pspec)
            }

            fn property(&self, id: usize, pspec: &glib::ParamSpec) -> glib::Value {
                Self::derived_property(self, id, pspec)
            }
        }
    }

    impl DownloadObject {
        pub fn new(
            filename: &str,
            size: &str,
            status: &str,
            progress: f64,
            speed: &str,
            url: &str,
            output_path: &str,
        ) -> Self {
            glib::Object::builder()
                .property("filename", filename)
                .property("size", size)
                .property("status", status)
                .property("progress", progress)
                .property("speed", speed)
                .property("url", url)
                .property("output-path", output_path)
                .build()
        }

        pub fn cancel(&self) {
            if let Some(token) = self.imp().cancel_token.borrow().as_ref() {
                token.cancel();
            }
            *self.imp().cancel_token.borrow_mut() = None;
        }

        pub fn set_cancel_token(&self, token: Option<tokio_util::sync::CancellationToken>) {
            *self.imp().cancel_token.borrow_mut() = token;
        }

        pub fn mark_removed(&self) {
            self.imp().removed.set(true);
        }

        pub fn is_removed(&self) -> bool {
            self.imp().removed.get()
        }
    }
}

use download_object::DownloadObject;

fn get_config_dir() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("oxidm");
    let _ = fs::create_dir_all(&path);
    path
}

fn get_config_file_path() -> PathBuf {
    get_config_dir().join("history.json")
}

/// One runtime serves the local API server and every download.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("failed to start the tokio runtime"))
}

/// Deletes the partial output file and resume state of a download that was removed.
fn cleanup_partial(output_path: &str) {
    let path = Path::new(output_path);
    let _ = fs::remove_file(DownloadState::state_file_path(path));
    let _ = fs::remove_file(path);
}

/// Picks a name in `dir` that does not overwrite an existing file. A file with a
/// resume state next to it belongs to an earlier attempt and keeps its name.
fn unique_output_path(dir: PathBuf, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() || DownloadState::state_file_path(&candidate).exists() {
        return candidate;
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.to_string());
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());

    for n in 1u32.. {
        let name = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn save_session(store: &gio::ListStore) {
    let mut records = Vec::new();
    for i in 0..store.n_items() {
        if let Some(item) = store.item(i) {
            if let Ok(download) = item.downcast::<DownloadObject>() {
                records.push(DownloadRecord {
                    filename: download.filename(),
                    size: download.size(),
                    status: download.status(),
                    progress: download.progress(),
                    speed: download.speed(),
                    url: download.url(),
                    output_path: download.output_path(),
                });
            }
        }
    }
    if let Ok(json) = serde_json::to_string_pretty(&records) {
        let _ = fs::write(get_config_file_path(), json);
    }
}

fn load_session(store: &gio::ListStore) {
    let path = get_config_file_path();
    if let Ok(data) = fs::read_to_string(path) {
        if let Ok(records) = serde_json::from_str::<Vec<DownloadRecord>>(&data) {
            for record in records {
                let status = if record.status == "Downloading" {
                    "Paused".to_string()
                } else {
                    record.status.clone()
                };

                let item = DownloadObject::new(
                    &record.filename,
                    &record.size,
                    &status,
                    record.progress,
                    "0.0 MB/s",
                    &record.url,
                    &record.output_path,
                );
                store.append(&item);
            }
        }
    }
}

fn main() {
    let app = Application::builder()
        .application_id("com.oxidown.gui")
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn spawn_download(item: DownloadObject, store: gio::ListStore, output_path: PathBuf) {
    let cancel_token = CancellationToken::new();
    item.set_cancel_token(Some(cancel_token.clone()));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<UiMessage>(100);
    let item_ui = item.clone();
    let store_ui = store.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                UiMessage::Progress(event) => {
                    let speed_mb = event.bytes_per_sec as f64 / 1_048_576.0;
                    let downloaded_mb = event.downloaded_bytes as f64 / 1_048_576.0;
                    let total_mb = event.total_bytes as f64 / 1_048_576.0;

                    let fraction = if event.total_bytes > 0 {
                        (event.downloaded_bytes as f64 / event.total_bytes as f64).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };

                    let size_str = if event.total_bytes > 0 {
                        format!("{:.1} / {:.1} MB", downloaded_mb, total_mb)
                    } else {
                        format!("{:.1} MB", downloaded_mb)
                    };

                    item_ui.set_progress(fraction);
                    item_ui.set_speed(format!("{:.2} MB/s", speed_mb));
                    item_ui.set_size(size_str);
                    item_ui.set_status("Downloading");
                }
                UiMessage::Paused => {
                    if item_ui.is_removed() {
                        cleanup_partial(&item_ui.output_path());
                    } else {
                        item_ui.set_status("Paused");
                        item_ui.set_speed("0.0 MB/s");
                    }
                    save_session(&store_ui);
                }
                UiMessage::Error(err) => {
                    eprintln!("oxidl: download of {} failed: {err}", item_ui.url());
                    if item_ui.is_removed() {
                        cleanup_partial(&item_ui.output_path());
                    } else {
                        item_ui.set_status("Error");
                        item_ui.set_speed("0.0 MB/s");
                    }
                    save_session(&store_ui);
                }
                UiMessage::Completed => {
                    item_ui.set_progress(1.0);
                    item_ui.set_status("Completed");
                    item_ui.set_speed("0.0 MB/s");
                    save_session(&store_ui);
                }
            }
        }
    });

    let task = DownloadTask::new(item.url(), output_path, 8).with_cancel_token(cancel_token);

    runtime().spawn(async move {
        let (tokio_tx, mut tokio_rx) = tokio::sync::mpsc::channel::<ProgressEvent>(100);

        // Forward progress at most every 100 ms, always passing the first and the final event.
        let tx_progress = tx.clone();
        let forwarder = tokio::spawn(async move {
            let mut last_update: Option<std::time::Instant> = None;
            while let Some(event) = tokio_rx.recv().await {
                let due = last_update
                    .map(|t| t.elapsed() >= std::time::Duration::from_millis(100))
                    .unwrap_or(true);
                let finished = event.total_bytes > 0 && event.downloaded_bytes == event.total_bytes;
                if due || finished {
                    last_update = Some(std::time::Instant::now());
                    let _ = tx_progress.send(UiMessage::Progress(event)).await;
                }
            }
        });

        let result = task.start_with_progress(tokio_tx).await;

        // Wait for the forwarder so a late progress event cannot overwrite the final status.
        let _ = forwarder.await;

        let message = match result {
            Ok(()) => UiMessage::Completed,
            Err(DownloadError::Paused) => UiMessage::Paused,
            Err(err) => UiMessage::Error(err.to_string()),
        };
        let _ = tx.send(message).await;
    });
}

fn build_ui(app: &Application) {
    StyleManager::default().set_color_scheme(ColorScheme::PreferDark);

    let header = HeaderBar::new();

    let url_entry = Entry::builder()
        .placeholder_text("Enter download URL...")
        .hexpand(true)
        .build();

    let add_button = Button::builder()
        .label("Add URL")
        .css_classes(["suggested-action"])
        .build();

    url_entry.connect_changed(|entry| entry.remove_css_class("error"));

    let pause_button = Button::builder().label("Pause").build();
    let resume_button = Button::builder().label("Resume").build();
    let delete_button = Button::builder().label("Remove").build();

    let toolbar = Box::new(Orientation::Horizontal, 6);
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);
    toolbar.set_margin_top(12);
    toolbar.set_margin_bottom(6);

    toolbar.append(&url_entry);
    toolbar.append(&add_button);
    toolbar.append(&pause_button);
    toolbar.append(&resume_button);
    toolbar.append(&delete_button);

    let store = gio::ListStore::new::<DownloadObject>();
    load_session(&store);

    let selection_model = SingleSelection::new(Some(store.clone()));

    let name_factory = SignalListItemFactory::new();
    name_factory.connect_setup(|_, list_item| {
        let label = Label::builder().halign(Align::Start).build();
        list_item.set_child(Some(&label));
    });
    name_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let label = list_item.child().and_downcast::<Label>().unwrap();
        item.bind_property("filename", &label, "label")
            .sync_create()
            .build();
    });

    let col_name = ColumnViewColumn::builder()
        .title("File Name")
        .factory(&name_factory)
        .expand(true)
        .build();

    let size_factory = SignalListItemFactory::new();
    size_factory.connect_setup(|_, list_item| {
        let label = Label::builder().halign(Align::Start).build();
        list_item.set_child(Some(&label));
    });
    size_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let label = list_item.child().and_downcast::<Label>().unwrap();
        item.bind_property("size", &label, "label")
            .sync_create()
            .build();
    });

    let col_size = ColumnViewColumn::builder()
        .title("Size")
        .factory(&size_factory)
        .fixed_width(160)
        .build();

    let status_factory = SignalListItemFactory::new();
    status_factory.connect_setup(|_, list_item| {
        let label = Label::builder().halign(Align::Start).build();
        list_item.set_child(Some(&label));
    });
    status_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let label = list_item.child().and_downcast::<Label>().unwrap();
        item.bind_property("status", &label, "label")
            .sync_create()
            .build();
    });

    let col_status = ColumnViewColumn::builder()
        .title("Status")
        .factory(&status_factory)
        .fixed_width(110)
        .build();

    let progress_factory = SignalListItemFactory::new();
    progress_factory.connect_setup(|_, list_item| {
        let pbar = ProgressBar::builder().show_text(true).build();
        list_item.set_child(Some(&pbar));
    });
    progress_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let pbar = list_item.child().and_downcast::<ProgressBar>().unwrap();
        item.bind_property("progress", &pbar, "fraction")
            .sync_create()
            .build();
    });

    let col_progress = ColumnViewColumn::builder()
        .title("Progress")
        .factory(&progress_factory)
        .fixed_width(140)
        .build();

    let speed_factory = SignalListItemFactory::new();
    speed_factory.connect_setup(|_, list_item| {
        let label = Label::builder().halign(Align::End).build();
        list_item.set_child(Some(&label));
    });
    speed_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let label = list_item.child().and_downcast::<Label>().unwrap();
        item.bind_property("speed", &label, "label")
            .sync_create()
            .build();
    });

    let col_speed = ColumnViewColumn::builder()
        .title("Speed")
        .factory(&speed_factory)
        .fixed_width(100)
        .build();

    let column_view = ColumnView::new(Some(selection_model.clone()));
    column_view.append_column(&col_name);
    column_view.append_column(&col_size);
    column_view.append_column(&col_status);
    column_view.append_column(&col_progress);
    column_view.append_column(&col_speed);

    let scrolled_window = ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(12)
        .build();

    let main_box = Box::new(Orientation::Vertical, 0);
    main_box.append(&header);
    main_box.append(&toolbar);
    main_box.append(&scrolled_window);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Oxidm")
        .default_width(850)
        .default_height(480)
        .content(&main_box)
        .build();

    let (url_tx, mut url_rx) = tokio::sync::mpsc::channel::<String>(32);

    let token_path = get_config_dir().join("api_token");
    match server::load_or_create_token(&token_path) {
        Ok(token) => {
            eprintln!("oxidl: API token file: {}", token_path.display());
            runtime().spawn(async move {
                if let Err(err) = server::start_server(url_tx, token).await {
                    eprintln!("oxidl: local API server stopped: {err}");
                }
            });
        }
        Err(err) => eprintln!("oxidl: local API server disabled, token unavailable: {err}"),
    }

    let store_http = store.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Some(url) = url_rx.recv().await {
            let dir = glib::user_special_dir(glib::UserDirectory::Downloads)
                .unwrap_or_else(glib::home_dir);
            let output_path = unique_output_path(dir, &suggest_filename(&url));
            let filename = output_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "download.bin".to_string());

            let path_str = output_path.to_string_lossy().to_string();

            let item = DownloadObject::new(
                &filename,
                "Connecting...",
                "Downloading",
                0.0,
                "0.0 MB/s",
                &url,
                &path_str,
            );

            store_http.append(&item);
            save_session(&store_http);

            spawn_download(item, store_http.clone(), output_path);
        }
    });

    let store_add = store.clone();
    let url_entry_add = url_entry.clone();
    let window_add = window.clone();

    add_button.connect_clicked(move |_| {
        let url = url_entry_add.text().trim().to_string();
        if url.is_empty() {
            return;
        }
        if validate_url(&url).is_err() {
            url_entry_add.add_css_class("error");
            return;
        }

        let default_name = suggest_filename(&url);

        let file_chooser = FileChooserNative::new(
            Some("Save Download As"),
            Some(&window_add),
            FileChooserAction::Save,
            Some("Save"),
            Some("Cancel"),
        );

        file_chooser.set_current_name(&default_name);

        let store_dialog = store_add.clone();
        let url_dialog = url.clone();
        let url_entry_dialog = url_entry_add.clone();

        file_chooser.connect_response(move |dialog, response| {
            if response == ResponseType::Accept {
                if let Some(file) = dialog.file() {
                    if let Some(path) = file.path() {
                        let display_name = path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "download.tmp".to_string());

                        let path_str = path.to_string_lossy().to_string();

                        let item = DownloadObject::new(
                            &display_name,
                            "Connecting...",
                            "Downloading",
                            0.0,
                            "0.0 MB/s",
                            &url_dialog,
                            &path_str,
                        );

                        store_dialog.append(&item);
                        save_session(&store_dialog);
                        url_entry_dialog.set_text("");

                        spawn_download(item, store_dialog.clone(), path);
                    }
                }
            }
            dialog.destroy();
        });

        file_chooser.show();
    });

    let selection_pause = selection_model.clone();
    let store_pause = store.clone();
    pause_button.connect_clicked(move |_| {
        let selected_pos = selection_pause.selected();
        if selected_pos != gtk4::INVALID_LIST_POSITION {
            if let Some(item) = selection_pause.item(selected_pos) {
                let download = item.downcast::<DownloadObject>().unwrap();
                download.cancel();
                save_session(&store_pause);
            }
        }
    });

    let selection_resume = selection_model.clone();
    let store_resume = store.clone();
    resume_button.connect_clicked(move |_| {
        let selected_pos = selection_resume.selected();
        if selected_pos != gtk4::INVALID_LIST_POSITION {
            if let Some(item) = selection_resume.item(selected_pos) {
                let download = item.downcast::<DownloadObject>().unwrap();
                if download.status() == "Paused" || download.status() == "Error" {
                    download.set_status("Downloading");
                    let output_path = PathBuf::from(download.output_path());
                    spawn_download(download, store_resume.clone(), output_path);
                    save_session(&store_resume);
                }
            }
        }
    });

    let store_delete = store.clone();
    let selection_delete = selection_model.clone();
    delete_button.connect_clicked(move |_| {
        let selected_pos = selection_delete.selected();
        if selected_pos != gtk4::INVALID_LIST_POSITION {
            if let Some(item) = selection_delete.item(selected_pos) {
                let download = item.downcast::<DownloadObject>().unwrap();
                let status = download.status();
                download.mark_removed();
                if status == "Downloading" {
                    // The worker reports back after it stops and the handler deletes the partial data.
                    download.cancel();
                } else if status != "Completed" {
                    cleanup_partial(&download.output_path());
                }
            }
            store_delete.remove(selected_pos);
            save_session(&store_delete);
        }
    });

    window.present();
}