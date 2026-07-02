pub use mime::*;

mod error;
mod parameter;
mod request;
mod response;

pub use error::Error;
pub use parameter::{Parameter, ParameterDTO, ParameterType};
pub use request::{Agent, Client, Method};
pub use response::{ParsedResponse, RawResponse, header_map_to_hash_map};

#[cfg(test)]
mod testing {
    use crate::error::Result;

    use std::fs;
    use std::io::{Read, Seek};

    /// Copies bytes from the 16x16.jpg from the multipart directly into a new file to check for correctness
    fn _copy_result() -> Result<()> {
        let test_file = "./scripts/output-multipart-mixed.txt";
        let mut file = fs::File::open(test_file)?;
        file.seek(std::io::SeekFrom::Start(0x190))?;
        let mut buffer: &mut [u8] = &mut [0; 0x1C19];
        file.read(&mut buffer)?;
        fs::write("./scripts/output.jpg", buffer)?;
        Ok(())
    }
}
