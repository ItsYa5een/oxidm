# Oxi Download Manager (Oxidm / ODM)

A fast, modern, multi-threaded download manager written in Rust, GTK4, and Libadwaita.

## Features

* **Accelerated Downloads:** Multi-threaded download engine powered by Tokio.
* **Native Interface:** Responsive GTK4 and Libadwaita UI with dark mode support.
* **Live Monitoring:** Real-time feedback for transfer speeds, ETA, and progress bars.
* **Task Management:** Full control over downloads with pause, resume, and queue ordering.
* **Resource Efficient:** Low CPU and memory footprint enabled by Rust.

## Tech Stack

* **Language:** Rust
* **UI Toolkit:** GTK4, Libadwaita (`libadwaita-rs`)
* **Async Runtime:** Tokio
* **HTTP Client:** Reqwest

## Prerequisites

Ensure your system includes the required development dependencies:

### Arch Linux

```bash
sudo pacman -S rustup gtk4 libadwaita

```

### Ubuntu / Debian

```bash
sudo apt update
sudo apt install build-essential libgtk-4-dev libadwaita-1-dev

```

## Installation & Building

1. Clone the repository:
```bash
git clone https://github.com/YOUR_GITHUB_USERNAME/oxidown.git
cd oxidown

```


2. Build and run the GUI application:
```bash
cargo run --release --bin oxidl-gui

```



## Usage

1. Launch **Oxi Download Manager**.
2. Paste a direct file download link into the URL field.
3. Click **Add URL** to start downloading.
4. Use the action buttons to pause, resume, or remove selected transfers.

## Contributing

Pull requests and issue reports are welcome. Feel free to open an issue to discuss proposed feature additions.

## License

This project is licensed under the MIT License.
