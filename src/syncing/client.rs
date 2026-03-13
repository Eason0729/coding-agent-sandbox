use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use crate::syncing::proto::{DirMetadata, FileMetadata, FuseEntry, Request, Response};

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

    pub fn put_object(&mut self, data: &[u8]) -> Result<u64, ClientError> {
        let response = self.send_request(Request::PutObject {
            data: data.to_vec(),
        })?;
        match response {
            Response::PutObject { id } => Ok(id),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn get_object(&mut self, id: u64) -> Result<Vec<u8>, ClientError> {
        let response = self.send_request(Request::GetObject { id })?;
        match response {
            Response::GetObject { data } => Ok(data),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Server("Unexpected response".to_string())),
        }
    }

    pub fn put_file(
        &mut self,
        path: PathBuf,
        data: Vec<u8>,
        meta: FileMetadata,
    ) -> Result<u64, ClientError> {
        let response = self.send_request(Request::PutFile { path, data, meta })?;
        match response {
            Response::PutFile { id } => Ok(id),
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

    pub fn put_dir(&mut self, path: PathBuf, meta: DirMetadata) -> Result<(), ClientError> {
        let response = self.send_request(Request::PutDir { path, meta })?;
        match response {
            Response::PutDir => Ok(()),
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
