use std::{fs, io};
use std::path::{Path, PathBuf};
use std::time::Duration;
use ecow::{eco_format, EcoString};

use tempfile::{NamedTempFile};
use ureq;
use ureq::{Error, Response};
use url::Url;

use typst::diag::{FileError, FileResult};
use typst::diag::FileError::Other;

struct AssetMirror {
    root: PathBuf,
}

impl AssetMirror {
    fn new(path: PathBuf) -> AssetMirror {
        return AssetMirror { root: path };
    }

    fn path_for<'a>(&self, url: &Url) -> PathBuf {
        self.root.as_path()
            .join(url.host_str().unwrap())
            .join(&url.path()[1..])
    }
}


#[cfg(test)]
mod tests_asset_mirror {
    use super::*;

    #[test]
    fn path_for_basic() {
        let url = Url::parse("https://example.com/a/b/doc.typ").unwrap();
        let asset_mirror = AssetMirror::new(PathBuf::from("/tmp/typst"));
        let ret = asset_mirror.path_for(&url);
        let expt = PathBuf::from("/tmp/typst/example.com/a/b/doc.typ");
        assert_eq!(ret, expt);
    }

    #[test]
    fn path_for_with_url_params() {
        let url = Url::parse("https://example.com/a/b/doc.typ?q=1234").unwrap();
        let asset_mirror = AssetMirror::new(PathBuf::from("/tmp/typst"));
        let ret = asset_mirror.path_for(&url);
        let expt = PathBuf::from("/tmp/typst/example.com/a/b/doc.typ");
        assert_eq!(ret, expt);
    }

    #[test]
    fn path_for_with_url_with_port() {
        let url = Url::parse("https://example.com:9876/a/b/doc.typ").unwrap();
        let asset_mirror = AssetMirror::new(PathBuf::from("/tmp/typst"));
        let ret = asset_mirror.path_for(&url);
        let expt = PathBuf::from("/tmp/typst/example.com/a/b/doc.typ");
        assert_eq!(ret, expt);
    }

}


pub struct HTTPRemoteAssetFetcher {
    _agent: ureq::Agent,
    mirror: AssetMirror,
}

fn other_err(msg: EcoString) -> FileError {
    Other(Some(msg))
}

impl HTTPRemoteAssetFetcher {

    pub fn new(root: PathBuf) -> HTTPRemoteAssetFetcher {
        let agent = ureq::AgentBuilder::new()
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .build();
        HTTPRemoteAssetFetcher {
            _agent: agent,
            mirror: AssetMirror::new(root),
        }
    }

    fn _create_named_temp_file(&self) -> FileResult<NamedTempFile> {
        let temp_file_res = NamedTempFile::new();
        temp_file_res.map_err(|err| other_err(eco_format!("Cannot create temporary file: {}", err)))
    }

    fn _download_response_in_temp_file(&self, resp: Response, url: &Url) -> FileResult<NamedTempFile>{
        let mut file = self._create_named_temp_file()?;
        let copy_res = io::copy(resp.into_reader().as_mut(),
                                &mut file);

        copy_res.map_err(|_| other_err(eco_format!("Error while downloading {}.", url)))
            .map( |_| file)
    }

    fn _move_file(&self, from: &Path, to: &Path) -> FileResult<()>{
        if let Some(parent) = to.parent(){
            let res = fs::create_dir_all(parent);
            if res.is_err() {
                return Err(other_err(eco_format!("Could not create directory {}", parent.to_str().unwrap_or(""))));
            }
        }
        let mv_resp = fs::rename(from, to);
        if mv_resp.is_err() {
            _ = fs::remove_file(to);
            return Err(other_err(eco_format!("Could not move file {} in expected location {}",
                from.to_str().unwrap_or(""),
                to.to_str().unwrap_or("")
            )));
        }
        Ok(())
    }

    fn download_response(&self, resp: Response, url: &Url) -> FileResult<PathBuf> {
        let temp_file = self._download_response_in_temp_file(resp, url)?;
        let file = self.mirror.path_for(url);
        self._move_file(temp_file.path(), file.as_path())?;
        Ok(file)
    }

    pub fn fetch(&self, url: &Url) -> FileResult<PathBuf> {
        let res = self._agent.get(url.as_str()).call();
        match res {
            Ok(response) => self.download_response(response, url),
            Err(Error::Status(code, _)) => Err(other_err(eco_format!("Error {} downloding asset at {}", code, url))),
            Err(_) => Err(other_err(eco_format!("Connection error to {}", url))),
        }
    }
}


#[cfg(test)]
mod tests_http_remote_asset_fetcher {
    use super::*;

    #[test]
    fn create_http_remote_asset_fetcher() {
        HTTPRemoteAssetFetcher::new(PathBuf::from("/tmp/toto"));
    }

    #[test]
    fn download_response(){
        let fetcher = HTTPRemoteAssetFetcher::new(PathBuf::from("/tmp/toto"));
        let url = Url::parse("https://example.com/foo/bar/toto.typ").unwrap();
        let resp = Response::new(200, "OK", "houray");
        fetcher.download_response(resp.unwrap(), & url).unwrap();
    }
}
