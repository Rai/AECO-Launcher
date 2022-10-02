use super::constants::*;
use crate::message::{GUIMessage, PatchMessage, PatchStatus};
use aeco_patch_config::fsobject::*;
use aeco_patch_config::status::ServerStatus;
use futures_util::StreamExt;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
};

const UPDATE_FILE_EXTENSION: &str = "aecoupdate";

pub struct PatchWorker {
    tx: Sender<PatchMessage>,
    rx: Receiver<GUIMessage>,
    self_dir: PathBuf,
    self_exe: PathBuf,
    client: reqwest::Client,
    server_url: reqwest::Url,
    game_base_url: reqwest::Url,
    game_zip_url: reqwest::Url,
    patchlist_url: reqwest::Url,
    status_url: reqwest::Url,
    patch_url: reqwest::Url,
    runtime: tokio::runtime::Runtime,
    updated_patcher: Option<PathBuf>,
}

impl PatchWorker {
    pub fn new(
        sender: Sender<PatchMessage>,
        receiver: Receiver<GUIMessage>,
    ) -> Result<Self, String> {
        let self_exe = std::env::current_exe().map_err(|err| err.to_string())?;
        let self_dir = self_exe
            .parent()
            .ok_or_else(|| "No parent directory for the launcher was found.".to_string())?
            .to_path_buf();
        let server_url = reqwest::Url::parse(PATCH_SERVER).map_err(|err| err.to_string())?;
        let game_base_url = server_url.join(BASE_DIR).map_err(|err| err.to_string())?;
        let game_zip_url = game_base_url
            .join(BASE_ZIP)
            .map_err(|err| err.to_string())?;
        let meta_url = server_url.join(META_DIR).map_err(|err| err.to_string())?;
        let patchlist_url = meta_url.join(PATCHLIST).map_err(|err| err.to_string())?;
        let status_url = meta_url.join(STATUS).map_err(|err| err.to_string())?;
        let patch_url = server_url.join(PATCH_DIR).map_err(|err| err.to_string())?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| err.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| err.to_string())?;

        Ok(Self {
            tx: sender,
            rx: receiver,
            self_dir,
            self_exe,
            client,
            server_url,
            game_base_url,
            game_zip_url,
            patchlist_url,
            status_url,
            patch_url,
            runtime,
            updated_patcher: None,
        })
    }

    /// Send a message to the GUI
    fn send(&self, message: PatchMessage) {
        if let Err(why) = self.tx.send(message) {
            eprintln!("Could not send message from PatchWorker to GUI: {why}");
        }
    }

    /// Send an error to the GUI
    fn send_error(&self, text: String) {
        self.send(PatchMessage::Error(text));
    }

    /// Send download information to the GUI
    fn send_download(&self, text: String, percentage: f32) {
        self.send(PatchMessage::Downloading(text, percentage));
    }

    /// Send connecting information to the GUI
    fn send_connecting(&self, text: String) {
        self.send(PatchMessage::Connecting(text));
    }

    /// Send information about the result of the patch routine to the GUI
    fn send_status(&self, status: PatchStatus) {
        self.send(PatchMessage::PatchStatus(status));
        self.clear_recv();
    }

    fn recv(&self) -> Result<GUIMessage, std::sync::mpsc::RecvError> {
        self.rx.recv()
    }

    fn clear_recv(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    pub fn run(&mut self) {
        self.main_loop();
    }

    fn main_loop(&mut self) {
        // Start by performing the patch check
        let mut message = GUIMessage::Retry;
        loop {
            match message {
                GUIMessage::Retry => {
                    self.send_status(PatchStatus::Working);
                    if let Err(why) = self.patch_routine() {
                        self.send_status(PatchStatus::Error);
                        eprintln!("{why}");
                    }
                }
                GUIMessage::Play => {
                    match self.start_game() {
                        Ok(_) => {
                            // The game is running and we can exit
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            self.send_status(PatchStatus::GameLaunched);
                            return;
                        }
                        Err(why) => {
                            // Could not launch the game, need to stay open to inform user
                            self.send_status(PatchStatus::Error);
                            eprintln!("Failed to launch game: {why}");
                        }
                    }
                }
            }

            message = match self.recv() {
                Ok(m) => m,
                Err(why) => {
                    eprintln!("{why}");
                    return;
                }
            };
        }
    }

    fn patch_routine(&mut self) -> Result<(), String> {
        self.check_patcher_aecoupdate()?;

        self.send_connecting("Checking server status".to_string());
        let server_status = self.download_server_status()?;

        match server_status {
            ServerStatus::Online => self.send_connecting("Server is online".to_string()),
            ServerStatus::Maintenance => {
                self.send_connecting("Server is down for maintenance".to_string());
                return Err("Server is down for maintenance".to_string());
            }
        }

        // Make sure the game is installed, and install it if not
        self.ensure_game_installed()?;

        // Get patch information from the patch server
        let patch = self.download_patch_metadata()?;

        // Apply patches for all platforms and for this specific platform
        for platform in ["all", &get_platform()] {
            // Compare local files against the patch data, and update files if needed
            if let Some(platform_dir) = subdir_by_name(&patch, platform) {
                self.check_platform_patches(platform_dir)?;
            } else {
                println!("No patch directory found for platform \'{platform}\'");
            }
        }

        self.send_status(PatchStatus::Finished);

        // Open the new patcher if there is one
        if let Some(p) = &self.updated_patcher {
            match subprocess::Popen::create(&[p], subprocess::PopenConfig::default()) {
                Ok(mut popen) => {
                    // End current patcher
                    popen.detach();
                    std::process::exit(0);
                }
                Err(why) => {
                    self.send_error("Could not start updated launcher".to_string());
                    return Err(format!("Could not start updated launcher: {}", why));
                }
            }
        }

        Ok(())
    }

    /// Checks whether the game is in the same directory as this program
    fn is_game_present(&self) -> bool {
        let game_path = self.self_dir.join(GAME_EXE);
        game_path.is_file()
    }

    /// Downloads the base game and returns it in a temporary file
    fn download_base(&self) -> Result<std::fs::File, String> {
        let response = match self
            .runtime
            .block_on(self.client.get(self.game_zip_url.clone()).send())
        {
            Ok(x) => x,
            Err(why) => {
                self.send_error("Failed to download game base".to_string());
                return Err(why.to_string());
            }
        };

        let status = response.status();
        if !status.is_success() {
            self.send_error("Failed to download game base".to_string());
            return Err(status.to_string());
        }

        let mut file = match tempfile::tempfile_in(&self.self_dir) {
            Ok(x) => x,
            Err(why) => {
                self.send_error("Failed to create game base".to_string());
                return Err(why.to_string());
            }
        };

        let total_size = response.content_length();
        let mut downloaded_size = 0u64;

        let mut stream = response.bytes_stream();

        while let Some(stream_result) = self.runtime.block_on(stream.next()) {
            let bytes = match stream_result {
                Ok(x) => x,
                Err(why) => {
                    self.send_error("Failed while reading game base stream".to_string());
                    return Err(why.to_string());
                }
            };

            if let Err(why) = file.write_all(&bytes) {
                self.send_error("Failed while writing base game to disk".to_string());
                return Err(why.to_string());
            }

            downloaded_size += bytes.len() as u64;
            let pretty_downloaded = byte_string(downloaded_size);

            if let Some(total_size) = total_size {
                downloaded_size = downloaded_size.min(total_size);
                let progress = downloaded_size as f32 / total_size as f32;
                let pretty_total = byte_string(total_size);
                self.send_download(
                    format!("Downloading base game ({pretty_downloaded} / {pretty_total})"),
                    progress,
                );
            } else {
                self.send_download(format!("Downloading base game ({pretty_downloaded})"), 1.);
            }
        }

        self.send_download("Finished downloading base game".to_string(), 1.);

        Ok(file)
    }

    /// Unpacks the base game ZIP to the same directory as this program
    fn unpack_base(&self, base_file: std::fs::File) -> Result<(), String> {
        let mut archive = match zip::read::ZipArchive::new(base_file) {
            Ok(a) => a,
            Err(why) => {
                self.send_error("Failed to extract base game".to_string());
                return Err(why.to_string());
            }
        };

        self.send_download("Extracting base game".to_string(), 0.);

        // Modified from zip/src/read.rs:extract
        // to provide real-time feedback to the GUI
        let total_archive_count = archive.len();

        // Calculate the total number of bytes to be extracted
        let mut total_archive_bytes = 0;
        for file_number in 0..total_archive_count {
            let file = match archive.by_index(file_number) {
                Ok(f) => f,
                Err(why) => {
                    self.send_error("Failed to read base game".to_string());
                    return Err(why.to_string());
                }
            };
            total_archive_bytes += file.size();
        }

        let pretty_total = byte_string(total_archive_bytes);
        let mut decompressed_bytes = 0;
        for file_number in 0..total_archive_count {
            // Report progress in terms of bytes extracted
            let progress = decompressed_bytes as f32 / total_archive_bytes as f32;
            let pretty_decompressed = byte_string(decompressed_bytes);
            self.send_download(
                format!(
                    "Extracting file {} of {} ({pretty_decompressed} / {pretty_total})",
                    file_number + 1,
                    total_archive_count
                ),
                progress,
            );

            let mut file = match archive.by_index(file_number) {
                Ok(f) => f,
                Err(why) => {
                    self.send_error("Failed to extract base game".to_string());
                    return Err(why.to_string());
                }
            };

            let filepath = match file.enclosed_name() {
                Some(x) => x,
                None => {
                    self.send_error("Failed to extract base game:\nInvalid file path".to_string());
                    return Err("Invalid file path".to_string());
                }
            };

            let outpath = self.self_dir.join(filepath);

            if file.name().ends_with('/') {
                if let Err(why) = std::fs::create_dir_all(&outpath) {
                    self.send_error("Failed to extract base game".to_string());
                    return Err(why.to_string());
                }
            } else {
                if let Some(p) = outpath.parent() {
                    if !p.exists() {
                        if let Err(why) = std::fs::create_dir_all(&p) {
                            self.send_error("Failed to extract base game".to_string());
                            return Err(why.to_string());
                        }
                    }
                }
                let mut outfile = match std::fs::File::create(&outpath) {
                    Ok(f) => f,
                    Err(why) => {
                        self.send_error("Failed to extract base game".to_string());
                        return Err(why.to_string());
                    }
                };
                if let Err(why) = std::io::copy(&mut file, &mut outfile) {
                    self.send_error("Failed to extract base game".to_string());
                    return Err(why.to_string());
                }
            }
            // Get and Set permissions
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = file.unix_mode() {
                    if let Err(why) =
                        std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode))
                    {
                        self.send_error("Failed to extract base game".to_string());
                        return Err(why.to_string());
                    }
                }
            }

            decompressed_bytes += file.size();
        }

        self.send_download("Finished installing base game".to_string(), 1.);

        Ok(())
    }

    /// Checks whether the game is installed and installs it if not
    fn ensure_game_installed(&self) -> Result<(), String> {
        self.send_download("Checking game installation".to_string(), 1.);
        if !self.is_game_present() {
            println!("Downloading game since it is not installed.");
            let base_file = self.download_base()?;
            self.unpack_base(base_file)?;
        }

        Ok(())
    }

    fn download_server_status(&self) -> Result<ServerStatus, String> {
        let response = match self
            .runtime
            .block_on(self.client.get(self.status_url.clone()).send())
        {
            Ok(x) => x,
            Err(why) => {
                self.send_error("Failed to get server status".to_string());
                return Err(why.to_string());
            }
        };

        let statuscode = response.status();
        if !statuscode.is_success() {
            self.send_error("Failed to get server status".to_string());
            return Err(statuscode.to_string());
        }

        let json_bytes = match self.runtime.block_on(response.bytes()) {
            Ok(b) => b,
            Err(why) => {
                self.send_error("Failed to get server status".to_string());
                return Err(why.to_string());
            }
        };

        let server_status = match serde_json::from_slice::<ServerStatus>(&json_bytes) {
            Ok(p) => p,
            Err(why) => {
                self.send_error("Failed to parse server status".to_string());
                return Err(why.to_string());
            }
        };

        Ok(server_status)
    }

    /// Downloads the patchlist and returns the parsed result
    fn download_patch_metadata(&self) -> Result<Directory, String> {
        let response = match self
            .runtime
            .block_on(self.client.get(self.patchlist_url.clone()).send())
        {
            Ok(x) => x,
            Err(why) => {
                self.send_error("Failed to get patch data".to_string());
                return Err(why.to_string());
            }
        };

        let status = response.status();
        if !status.is_success() {
            self.send_error("Failed to get patch data".to_string());
            return Err(status.to_string());
        }

        let json_bytes = match self.runtime.block_on(response.bytes()) {
            Ok(b) => b,
            Err(why) => {
                self.send_error("Failed to get patch data".to_string());
                return Err(why.to_string());
            }
        };

        let patch_dir = match serde_json::from_slice::<Directory>(&json_bytes) {
            Ok(p) => p,
            Err(why) => {
                self.send_error("Failed to parse patch data".to_string());
                return Err(why.to_string());
            }
        };

        Ok(patch_dir)
    }

    /// Checks files to be patched, patching them if necessary
    fn check_platform_patches(&mut self, dir: &Directory) -> Result<(), String> {
        let check_platform = &dir.name;

        // The URL to start at needs to be patch/platform because that is where
        // platform specific files are stored
        let platform_net_path = self
            .patch_url
            .join(&format!("{check_platform}/"))
            .map_err(|why| why.to_string())?;

        let total_files = get_total_files_in_patch(dir);
        let disk_dir = self.self_dir.clone();
        let checked_files = match self.check_patches_dir(
            dir,
            disk_dir,
            platform_net_path,
            check_platform,
            0,
            total_files,
        ) {
            Ok(n) => n,
            Err(why) => {
                self.send_error("Failed to check local files".to_string());
                return Err(why);
            }
        };

        if checked_files != total_files {
            eprintln!(
                "Checked files: {checked_files}; total files: {total_files}. These should match."
            );
        }

        self.send_download(format!("{total_files} files checked."), 1.);

        Ok(())
    }

    /// Iterates through a directory for files to be patched
    fn check_patches_dir<P>(
        &mut self,
        dir: &Directory,
        disk_dir: P,
        net_path: reqwest::Url,
        platform: &str,
        completed_files: usize,
        total_files: usize,
    ) -> Result<usize, String>
    where
        P: AsRef<Path>,
    {
        let mut completed_files = completed_files;
        for child in &dir.children {
            completed_files = match child {
                FSObject::File(file) => self.check_patches_file(
                    file,
                    disk_dir.as_ref().join(&file.name),
                    net_path.join(&file.name).map_err(|why| why.to_string())?,
                    platform,
                    completed_files,
                    total_files,
                )?,
                FSObject::Directory(d) => self.check_patches_dir(
                    d,
                    disk_dir.as_ref().join(&d.name),
                    net_path
                        .join(&format!("{}/", &d.name)) // Dir URLS need / to be parsed correctly
                        .map_err(|why| why.to_string())?,
                    platform,
                    completed_files,
                    total_files,
                )?,
                FSObject::Archive(a) => self.check_patches_archive(
                    a,
                    disk_dir.as_ref().join(&a.name).with_extension("hed"),
                    disk_dir.as_ref().join(&a.name).with_extension("dat"),
                    // Dir URLs need / to be parsed correctly, and archives are stored online as .archive
                    net_path
                        .join(&format!("{}.archive/", &a.name))
                        .map_err(|why| why.to_string())?,
                    platform,
                    completed_files,
                    total_files,
                )?,
            }
        }

        Ok(completed_files)
    }

    /// Checks whether a file should be patched, patching it if necessary
    fn check_patches_file<P>(
        &mut self,
        file: &File,
        disk_file: P,
        net_file: reqwest::Url,
        platform: &str,
        mut completed_files: usize,
        total_files: usize,
    ) -> Result<usize, String>
    where
        P: AsRef<Path>,
    {
        let file_to_check = disk_file.as_ref();
        let mut file_to_write = file_to_check.to_path_buf();

        let progress = completed_files as f32 / total_files as f32;
        self.send_download(
            format!(
                "Checking file {} / {} for platform \'{platform}\'",
                completed_files + 1,
                total_files
            ),
            progress,
        );

        if !file_to_write.exists() {
            println!("Downloading new file {net_file} -> {:?}", &file_to_write);
            let file_bytes = self.download_patched_file(net_file)?;
            std::fs::write(file_to_write, file_bytes).map_err(|why| why.to_string())?;
        } else {
            let file_matches = {
                let disk_data = std::fs::read(&file_to_check).map_err(|why| why.to_string())?;
                let disk_file_data = File::new(&file.name, &disk_data);
                file.digest == disk_file_data.digest
            };

            let is_self = file_to_check == self.self_exe;

            // If the patched file is this program, don't try to overwrite it
            // while it is running. Instead, save it as a different file name
            // and move it later.
            if is_self {
                file_to_write = self.get_self_aecoupdate_path()?;
            }

            if !file_matches {
                println!("Updating {net_file} -> {:?}", &file_to_write);
                let file_bytes = self.download_patched_file(net_file)?;
                std::fs::write(&file_to_write, file_bytes).map_err(|why| why.to_string())?;
                // If we got the file successfully, and it is a replacement for
                // this program, save the path to the new one for later so we
                // can switch to it.
                // TODO: make sure the file is executable on unix
                if is_self {
                    self.updated_patcher = Some(file_to_write);
                }
            }
        }

        completed_files += 1;

        Ok(completed_files)
    }

    /// Iterates through an archive checking for files to be patched, patching them if necessary
    fn check_patches_archive<P1, P2>(
        &self,
        archive: &Archive,
        hed_path: P1,
        dat_path: P2,
        net_path: reqwest::Url,
        platform: &str,
        completed_files: usize,
        total_files: usize,
    ) -> Result<usize, String>
    where
        P1: AsRef<Path>,
        P2: AsRef<Path>,
    {
        let mut disk_archive =
            aeco_archive::Archive::open_pair(dat_path.as_ref(), hed_path.as_ref())
                .map_err(|_| format!("Couldn't open archive {}", &archive.name))?;
        let mut changes_made = false;

        let mut completed_files = completed_files;

        for file in &archive.files {
            let progress = completed_files as f32 / total_files as f32;
            self.send_download(
                format!(
                    "Checking file {} / {} for platform \'{platform}\'",
                    completed_files + 1,
                    total_files
                ),
                progress,
            );
            let file_matches = match disk_archive.get_file(&file.name) {
                Ok(archive_data) => {
                    // File is present
                    let archive_file = File::new(&file.name, &archive_data);
                    file.digest == archive_file.digest
                }
                Err(aeco_archive::ArchiveError::FileNotPresentError) => {
                    // The file is not present
                    false
                }
                Err(_) => {
                    // Some other error happened
                    return Err(format!("Failed while reading {}", archive.name));
                }
            };

            if !file_matches {
                let new_file_url = net_path.join(&file.name).map_err(|why| why.to_string())?;
                println!(
                    "Downloading {new_file_url} -> {:?} / {:?}",
                    dat_path.as_ref(),
                    hed_path.as_ref()
                );
                let new_file_bytes = self.download_patched_file(new_file_url)?;
                disk_archive
                    .add_file(&file.name, &new_file_bytes)
                    .map_err(|_| format!("Couldn't write to archive {}", &archive.name))?;
                changes_made = true;
            }

            completed_files += 1;
        }

        if changes_made {
            disk_archive
                .finalize()
                .map_err(|_| format!("Couldn't finalize archive {}", &archive.name))?;
            disk_archive
                .defrag()
                .map_err(|_| format!("Couldn't defrag archive {}", &archive.name))?;
        }

        Ok(completed_files)
    }

    /// Downloads a file and returns the resulting bytes
    fn download_patched_file(&self, net_file: reqwest::Url) -> Result<Vec<u8>, String> {
        let response = match self.runtime.block_on(self.client.get(net_file).send()) {
            Ok(x) => x,
            Err(why) => {
                self.send_error("Failed to get patched file".to_string());
                return Err(why.to_string());
            }
        };

        let status = response.status();
        if !status.is_success() {
            self.send_error("Failed to get patched file".to_string());
            return Err(status.to_string());
        }

        let bytes = match self.runtime.block_on(response.bytes()) {
            Ok(b) => b,
            Err(why) => {
                self.send_error("Failed to get patched file".to_string());
                return Err(why.to_string());
            }
        };

        Ok(bytes.to_vec())
    }

    fn start_game(&self) -> Result<(), String> {
        self.send_download("Let's play the game!".to_string(), 1.);

        Ok(())
    }

    fn check_patcher_aecoupdate(&self) -> Result<(), String> {
        // Get current extension, or finish if there is none
        let extension = match self.self_exe.extension() {
            Some(ext) => ext,
            None => {
                // Remove aecoupdate launcher if there is one
                self.remove_aecoupdate_file()?;
                return Ok(());
            }
        };

        // Finish if the current extension isn't the update extension
        if extension != UPDATE_FILE_EXTENSION {
            return Ok(());
        }

        // Remove the extension
        // This only removes the last extension, so if the file is supposed to
        // have an extension (e.g. ".exe") that part will remain, and it will
        // become the new extension
        let new_file_path = {
            let mut path = self.self_exe.clone();
            path.set_extension("");
            path
        };

        // Copy this program to the normal program filename
        // Try a few times, it is possible that the old process hasn't shut
        // down yet
        let retries = 5;
        for retry in 1..=retries {
            if let Err(why) = std::fs::copy(&self.self_exe, &new_file_path) {
                if retry == retries {
                    self.send_error("Failed to overwrite old patcher".to_string());
                    return Err(format!("Failed to overwrite patcher: {why}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            } else {
                break;
            }
        }

        // Open the restored launcher and close this one
        match subprocess::Popen::create(&[new_file_path], subprocess::PopenConfig::default()) {
            Ok(mut popen) => {
                // End current patcher
                popen.detach();
                std::process::exit(0)
            }
            Err(why) => {
                self.send_error("Could not start overwritten launcher".to_string());
                Err(format!("Could not start overwritten launcher: {why}"))
            }
        }
    }

    fn remove_aecoupdate_file(&self) -> Result<(), String> {
        let path = self.get_self_aecoupdate_path()?;
        if path.exists() {
            // Try a few times, it is possible that the old process hasn't shut
            // down yet
            let retries = 5;
            for retry in 1..=retries {
                if let Err(why) = std::fs::remove_file(&path) {
                    if retry == retries {
                        self.send_error("Failed to remove temporary launcher.".to_string());
                        return Err(format!("Failed to remove temporary launcher: {why}"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                } else {
                    break;
                }
            }
        }

        Ok(())
    }

    fn get_self_aecoupdate_path(&self) -> Result<PathBuf, String> {
        let current_name = self
            .self_exe
            .file_name()
            .ok_or_else(|| "Failed to get launcher file name".to_string())?
            .to_str()
            .ok_or_else(|| "Failed to read launcher file name as a string".to_string())?;
        let file_name = format!("{current_name}.{UPDATE_FILE_EXTENSION}");
        Ok(self.self_exe.with_file_name(file_name))
    }
}

/// Gets a Directory child from a Directory by name, if it is present
fn subdir_by_name<'a>(dir: &'a Directory, name: &str) -> Option<&'a Directory> {
    for child in &dir.children {
        if let FSObject::Directory(d) = child {
            if d.name == name {
                return Some(d);
            }
        }
    }
    None
}

/// Gets a platform string to represent the current platform
/// Some examples: `windows-x86_64`, `linux-x86`, `macos-aarch64`
fn get_platform() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!("{os}-{arch}")
}

fn get_total_files_in_patch(dir: &Directory) -> usize {
    let mut total_files = 0;

    for child in &dir.children {
        total_files += match child {
            FSObject::File(_) => 1,
            FSObject::Directory(d) => get_total_files_in_patch(d),
            FSObject::Archive(a) => a.files.len(),
        };
    }

    total_files
}

/// Format a quantity of bytes into a human readable string
fn byte_string<T>(bytes: T) -> String
where
    T: Into<u128>,
{
    byte_unit::Byte::from_bytes(bytes.into())
        .get_appropriate_unit(true) // binary units
        .to_string()
}
