use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use crate::syncing::proto::{FileMetadata, FuseEntry, Request, Response};

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialize(#[from] postcard::Error),
    #[error("Server error: {0}")]
    Server(String),
    #[error("Not found")]
    NotFound,
}

pub struct SyncClient {
    stream: std::os::unix::net::UnixStream,
}

impl SyncClient {
    pub fn connect(sock_path: &Path) -> Result<Self, ClientError> {
        let stream = std::os::unix::net::UnixStream::connect(sock_path)?;
        Ok(Self { stream })
    }

    fn send_request(&mut self, request: Request) -> Result<Response, ClientError> {
        let data = postcard::to_allocvec(&request)?;
        let length = (data.len() as u32).to_le_bytes();

        self.stream.write_all(&length)?;
        self.stream.write_all(&data)?;
        self.stream.flush()?;

        let mut length_buf = [0u8; 4];
        self.stream.read_exact(&mut length_buf)?;
        let length = u32::from_le_bytes(length_buf) as usize;

        let mut response_buf = vec![0u8; length];
        self.stream.read_exact(&mut response_buf)?;

        let response: Response = postcard::from_bytes(&response_buf)?;
        Ok(response)
    }

    pub fn ensure_file_object(
        &mut self,
        path: PathBuf,
        meta: FileMetadata,
    ) -> Result<(u64, PathBuf), ClientError> {
        let response = self.send_request(Request::EnsureFileObject { path, meta })?;
        match response {
            Response::EnsureFileObject { id, path } => Ok((id, path)),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn get_object_path(&mut self, id: u64) -> Result<PathBuf, ClientError> {
        let response = self.send_request(Request::GetObjectPath { id })?;
        match response {
            Response::GetObjectPath { path } => Ok(path),
            Response::NotFound => Err(ClientError::NotFound),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn upsert_file_entry(
        &mut self,
        path: PathBuf,
        object_id: u64,
        meta: FileMetadata,
    ) -> Result<(), ClientError> {
        let response = self.send_request(Request::UpsertFileEntry {
            path,
            object_id,
            meta,
        })?;
        match response {
            Response::UpsertFileEntry => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn put_file_meta(&mut self, path: PathBuf, meta: FileMetadata) -> Result<(), ClientError> {
        let response = self.send_request(Request::PutFileMeta { path, meta })?;
        match response {
            Response::PutFileMeta => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn get_file_meta(&mut self, path: PathBuf) -> Result<Option<FileMetadata>, ClientError> {
        let response = self.send_request(Request::GetFileMeta { path })?;
        match response {
            Response::GetFileMeta(meta) => Ok(meta),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn get_entry(&mut self, path: PathBuf) -> Result<Option<FuseEntry>, ClientError> {
        let response = self.send_request(Request::GetEntry { path })?;
        match response {
            Response::GetEntry(entry) => Ok(entry),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn delete_file(&mut self, path: PathBuf) -> Result<(), ClientError> {
        let response = self.send_request(Request::DeleteFile { path })?;
        match response {
            Response::DeleteFile => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn rename_file(&mut self, from: PathBuf, to: PathBuf) -> Result<(), ClientError> {
        let response = self.send_request(Request::RenameFile { from, to })?;
        match response {
            Response::RenameFile => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn put_dir(&mut self, path: PathBuf, meta: FileMetadata) -> Result<(), ClientError> {
        let response = self.send_request(Request::PutDir { path, meta })?;
        match response {
            Response::PutDir => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn put_symlink(
        &mut self,
        path: PathBuf,
        target: Vec<u8>,
        meta: FileMetadata,
    ) -> Result<(), ClientError> {
        let response = self.send_request(Request::PutSymlink { path, target, meta })?;
        match response {
            Response::PutSymlink => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn put_whiteout(&mut self, path: PathBuf) -> Result<(), ClientError> {
        let response = self.send_request(Request::PutWhiteout { path })?;
        match response {
            Response::PutWhiteout => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn delete_whiteout(&mut self, path: PathBuf) -> Result<(), ClientError> {
        let response = self.send_request(Request::DeleteWhiteout { path })?;
        match response {
            Response::DeleteWhiteout => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn read_dir_all(
        &mut self,
        path: PathBuf,
    ) -> Result<Vec<(PathBuf, FuseEntry)>, ClientError> {
        let response = self.send_request(Request::ReadDirAll { path })?;
        match response {
            Response::DirEntries(entries) => Ok(entries),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn list_whiteout_under(&mut self, path: PathBuf) -> Result<Vec<PathBuf>, ClientError> {
        let response = self.send_request(Request::ListWhiteoutUnder { path })?;
        match response {
            Response::WhiteoutPaths(paths) => Ok(paths),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn rename_tree(&mut self, from: PathBuf, to: PathBuf) -> Result<(), ClientError> {
        let response = self.send_request(Request::RenameTree { from, to })?;
        match response {
            Response::RenameTree => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn log_access(
        &mut self,
        path: PathBuf,
        operation: String,
        pid: u32,
    ) -> Result<(), ClientError> {
        let response = self.send_request(Request::LogAccess {
            path,
            operation,
            pid,
        })?;
        match response {
            Response::LogAccess => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn flush(&mut self) -> Result<(), ClientError> {
        let response = self.send_request(Request::Flush)?;
        match response {
            Response::Flush => Ok(()),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }
}
