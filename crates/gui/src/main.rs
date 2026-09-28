use gtk4::gio;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, ColumnView, ColumnViewColumn, Entry, HeaderBar, Label,
    Orientation, ProgressBar, ScrolledWindow, SignalListItemFactory, SingleSelection,
};
use libadwaita::{Application, ApplicationWindow, ColorScheme, StyleManager};
use oxidl_core::{DownloadTask, ProgressEvent};
use std::path::PathBuf;
use tokio::runtime::Runtime;

enum UiMessage {
    Progress(ProgressEvent),
    Error(String),
    Completed,
}

mod download_object {
    use gtk4::glib;
    use gtk4::subclass::prelude::*;

    glib::wrapper! {
        pub struct DownloadObject(ObjectSubclass<imp::DownloadObject>);
    }

    mod imp {
        use gtk4::glib;
        use gtk4::subclass::prelude::*;
        use std::cell::RefCell;

        #[derive(Default)]
        pub struct DownloadObject {
            pub filename: RefCell<String>,
            pub size: RefCell<String>,
            pub status: RefCell<String>,
            pub progress: RefCell<f64>,
            pub speed: RefCell<String>,
            pub url: RefCell<String>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for DownloadObject {
            const NAME: &'static str = "OxidlDownloadObject";
            type Type = super::DownloadObject;
        }

        impl ObjectImpl for DownloadObject {}
    }

    impl DownloadObject {
        pub fn new(filename: &str, size: &str, status: &str, progress: f64, speed: &str, url: &str) -> Self {
            let obj: Self = glib::Object::builder().build();
            let imp = obj.imp();
            *imp.filename.borrow_mut() = filename.to_string();
            *imp.size.borrow_mut() = size.to_string();
            *imp.status.borrow_mut() = status.to_string();
            *imp.progress.borrow_mut() = progress;
            *imp.speed.borrow_mut() = speed.to_string();
            *imp.url.borrow_mut() = url.to_string();
            obj
        }

        pub fn filename(&self) -> String {
            self.imp().filename.borrow().clone()
        }

        pub fn size(&self) -> String {
            self.imp().size.borrow().clone()
        }

        pub fn status(&self) -> String {
            self.imp().status.borrow().clone()
        }

        pub fn progress(&self) -> f64 {
            *self.imp().progress.borrow()
        }

        pub fn speed(&self) -> String {
            self.imp().speed.borrow().clone()
        }

        pub fn set_progress(&self, progress: f64) {
            *self.imp().progress.borrow_mut() = progress;
        }

        pub fn set_speed(&self, speed: &str) {
            *self.imp().speed.borrow_mut() = speed.to_string();
        }

        pub fn set_status(&self, status: &str) {
            *self.imp().status.borrow_mut() = status.to_string();
        }
    }
}

use download_object::DownloadObject;

fn main() {
    let app = Application::builder()
        .application_id("com.oxidown.gui")
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &Application) {
    let style_manager = StyleManager::default();
    style_manager.set_color_scheme(ColorScheme::PreferDark);

    let header = HeaderBar::new();

    let url_entry = Entry::builder()
        .placeholder_text("Enter download URL...")
        .hexpand(true)
        .build();

    let add_button = Button::builder()
        .label("Add URL")
        .css_classes(["suggested-action"])
        .build();

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
    let selection_model = SingleSelection::new(Some(store.clone()));

    let name_factory = SignalListItemFactory::new();
    name_factory.connect_setup(|_, list_item| {
        let label = Label::builder().halign(Align::Start).build();
        list_item.set_child(Some(&label));
    });
    name_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let label = list_item.child().and_downcast::<Label>().unwrap();
        label.set_text(&item.filename());
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
        label.set_text(&item.size());
    });

    let col_size = ColumnViewColumn::builder()
        .title("Size")
        .factory(&size_factory)
        .fixed_width(90)
        .build();

    let status_factory = SignalListItemFactory::new();
    status_factory.connect_setup(|_, list_item| {
        let label = Label::builder().halign(Align::Start).build();
        list_item.set_child(Some(&label));
    });
    status_factory.connect_bind(|_, list_item| {
        let item = list_item.item().and_downcast::<DownloadObject>().unwrap();
        let label = list_item.child().and_downcast::<Label>().unwrap();
        label.set_text(&item.status());
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
        pbar.set_fraction(item.progress());
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
        label.set_text(&item.speed());
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
        .title("Oxidown")
        .default_width(850)
        .default_height(480)
        .content(&main_box)
        .build();

    let store_add = store.clone();
    let url_entry_add = url_entry.clone();

    add_button.connect_clicked(move |_| {
        let url = url_entry_add.text().to_string();
        if url.is_empty() {
            return;
        }

        let raw_filename = url
            .split('/')
            .last()
            .unwrap_or("download.tmp")
            .to_string();

        let filename = if raw_filename.is_empty() {
            "download.tmp".to_string()
        } else {
            raw_filename
        };

        let item = DownloadObject::new(
            &filename,
            "Connecting...",
            "Downloading",
            0.0,
            "0.0 MB/s",
            &url,
        );

        let position = store_add.n_items();
        store_add.append(&item);
        url_entry_add.set_text("");

        let (tx, mut rx) = tokio::sync::mpsc::channel::<UiMessage>(100);

        let item_ui = item.clone();
        let store_ui = store_add.clone();

        gtk4::glib::MainContext::default().spawn_local(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    UiMessage::Progress(event) => {
                        let speed_mb = event.bytes_per_sec as f64 / 1_048_576.0;
                        let fraction = event.percentage / 100.0;
                        item_ui.set_progress(fraction);
                        item_ui.set_speed(&format!("{:.2} MB/s", speed_mb));
                        item_ui.set_status("Downloading");
                        store_ui.items_changed(position, 1, 1);
                    }
                    UiMessage::Error(_err) => {
                        item_ui.set_status("Error");
                        item_ui.set_speed("0.0 MB/s");
                        store_ui.items_changed(position, 1, 1);
                    }
                    UiMessage::Completed => {
                        item_ui.set_progress(1.0);
                        item_ui.set_status("Completed");
                        item_ui.set_speed("0.0 MB/s");
                        store_ui.items_changed(position, 1, 1);
                    }
                }
            }
        });

        let output_path = PathBuf::from(&filename);
        let task = DownloadTask::new(url, output_path, 8);

        std::thread::spawn(move || {
            let rt = Runtime::new().unwrap();
            rt.block_on(async move {
                let (tokio_tx, mut tokio_rx) =
                    tokio::sync::mpsc::channel::<ProgressEvent>(100);

                let tx_progress = tx.clone();
                tokio::spawn(async move {
                    while let Some(event) = tokio_rx.recv().await {
                        let _ = tx_progress.send(UiMessage::Progress(event)).await;
                    }
                });

                match task.start_with_progress(tokio_tx).await {
                    Ok(_) => {
                        let _ = tx.send(UiMessage::Completed).await;
                    }
                    Err(err) => {
                        let _ = tx.send(UiMessage::Error(err.to_string())).await;
                    }
                }
            });
        });
    });

    let store_delete = store.clone();
    let selection_delete = selection_model.clone();

    delete_button.connect_clicked(move |_| {
        let selected_pos = selection_delete.selected();
        if selected_pos != gtk4::INVALID_LIST_POSITION {
            store_delete.remove(selected_pos);
        }
    });

    window.present();
}