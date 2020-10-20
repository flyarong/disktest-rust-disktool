// -*- coding: utf-8 -*-
//
// disktest - Hard drive tester
//
// Copyright 2020 Michael Buesch <m@bues.ch>
//
// This program is free software; you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation; either version 2 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along
// with this program; if not, write to the Free Software Foundation, Inc.,
// 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA.
//

use crate::error::Error;
use crate::stream_aggregator::DtStreamAgg;
use crate::util::prettybyte;
use libc::ENOSPC;
use signal_hook;
use std::cmp::min;
use std::fs::File;
use std::io::{Read, Write, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const LOGTHRES: usize = 1024 * 1024 * 10;

pub struct Disktest<'a> {
    stream_agg:     DtStreamAgg,
    file:           &'a mut File,
    path:           &'a Path,
    abort:          Arc<AtomicBool>,
}

impl<'a> Disktest<'a> {
    pub fn new(seed:        &'a Vec<u8>,
               nr_threads:  usize,
               file:        &'a mut File,
               path:        &'a Path) -> Result<Disktest<'a>, Error> {

        let abort = Arc::new(AtomicBool::new(false));
        for sig in &[signal_hook::SIGTERM,
                     signal_hook::SIGINT] {
            if let Err(e) = signal_hook::flag::register(*sig, Arc::clone(&abort)) {
                return Err(Error::new(&format!("Failed to register signal {}: {}",
                                               sig, e)));
            }

        }
        let nr_threads = if nr_threads <= 0 { num_cpus::get() } else { nr_threads };
        return Ok(Disktest {
            stream_agg: DtStreamAgg::new(seed, nr_threads),
            file,
            path,
            abort,
        })
    }

    fn write_finalize(&mut self, bytes_written: u64) -> Result<(), Error> {
        println!("Done. Wrote {}. Syncing...", prettybyte(bytes_written));
        if let Err(e) = self.file.sync_all() {
            return Err(Error::new(&format!("Sync failed: {}", e)));
        }
        return Ok(());
    }

    pub fn write(&mut self, seek: u64, max_bytes: u64) -> Result<u64, Error> {
        println!("Writing {:?} ...", self.path);

        let mut bytes_left = max_bytes;
        let mut bytes_written = 0u64;
        let mut log_count = 0;

        self.stream_agg.activate();
        if let Err(e) = self.file.seek(SeekFrom::Start(seek)) {
            return Err(Error::new(&format!("File seek to {} failed: {}",
                                           seek, e.to_string())));
        }

        loop {
            // Get the next data chunk.
            let chunk = self.stream_agg.wait_chunk();
            let write_len = min(self.stream_agg.get_chunksize() as u64, bytes_left) as usize;

            // Write the chunk to disk.
            if let Err(e) = self.file.write_all(&chunk.data[0..write_len]) {
                if let Some(err_code) = e.raw_os_error() {
                    if err_code == ENOSPC {
                        self.write_finalize(bytes_written)?;
                        break; // End of device. -> Success.
                    }
                }
                self.write_finalize(bytes_written)?;
                return Err(Error::new(&format!("Write error: {}", e)));
            }

            // Account for the written bytes.
            bytes_written += write_len as u64;
            bytes_left -= write_len as u64;
            if bytes_left == 0 {
                self.write_finalize(bytes_written)?;
                break;
            }
            log_count += write_len;
            if log_count >= LOGTHRES {
                println!("Wrote {}.", prettybyte(bytes_written));
                log_count -= LOGTHRES;
            }

            if self.abort.load(Ordering::Relaxed) {
                self.write_finalize(bytes_written)?;
                return Err(Error::new("Aborted by signal!"));
            }
        }
        return Ok(bytes_written);
    }

    fn verify_finalize(&mut self, bytes_read: u64) -> Result<(), Error> {
        println!("Done. Verified {}.", prettybyte(bytes_read));
        return Ok(());
    }

    pub fn verify(&mut self, seek: u64, max_bytes: u64) -> Result<u64, Error> {
        println!("Reading {:?} ...", self.path);

        let mut bytes_left = max_bytes;
        let mut bytes_read = 0u64;
        let mut log_count = 0;

        let readbuf_len = self.stream_agg.get_chunksize();
        let mut buffer = vec![0; readbuf_len];
        let mut read_count = 0;

        self.stream_agg.activate();
        if let Err(e) = self.file.seek(SeekFrom::Start(seek)) {
            return Err(Error::new(&format!("File seek to {} failed: {}",
                                           seek, e.to_string())));
        }

        let mut read_len = min(readbuf_len as u64, bytes_left) as usize;
        loop {
            // Read the next chunk from disk.
            match self.file.read(&mut buffer[read_count..read_count+(read_len-read_count)]) {
                Ok(n) => {
                    read_count += n;

                    // Check if the read buffer is full, or if we are the the end of the disk.
                    assert!(read_count <= read_len);
                    if read_count == read_len || (read_count > 0 && n == 0) {
                        // Calculate and compare the read buffer to the pseudo random sequence.
                        let chunk = self.stream_agg.wait_chunk();
                        for i in 0..read_count {
                            if buffer[i] != chunk.data[i] {
                                return Err(Error::new(&format!("Data MISMATCH at Byte {}!",
                                                               bytes_read + i as u64)));
                            }
                        }

                        // Account for the read bytes.
                        bytes_read += read_count as u64;
                        bytes_left -= read_count as u64;
                        if bytes_left == 0 {
                            self.verify_finalize(bytes_read)?;
                            break;
                        }
                        log_count += read_count;
                        if log_count >= LOGTHRES {
                            println!("Verified {}.", prettybyte(bytes_read));
                            log_count -= LOGTHRES;
                        }
                        read_count = 0;
                        read_len = min(readbuf_len as u64, bytes_left) as usize;
                    }

                    // End of the disk?
                    if n == 0 {
                        self.verify_finalize(bytes_read)?;
                        break;
                    }
                },
                Err(e) => {
                    return Err(Error::new(&format!("Read error at {}: {}",
                                                   prettybyte(bytes_read), e)));
                },
            };

            if self.abort.load(Ordering::Relaxed) {
                self.verify_finalize(bytes_read)?;
                return Err(Error::new("Aborted by signal!"));
            }
        }
        return Ok(bytes_read);
    }
}

#[cfg(test)]
mod tests {
    use crate::stream::DtStream;
    use std::path::Path;
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_basic() {
        let mut tfile = NamedTempFile::new().unwrap();
        let pstr = String::from(tfile.path().to_str().unwrap());
        let path = Path::new(&pstr);
        let mut file = tfile.as_file_mut();
        let mut loc_file = file.try_clone().unwrap();
        let seed = vec![42, 43, 44, 45];
        let nr_threads = 2;
        let mut dt = Disktest::new(&seed, nr_threads, &mut file, &path).unwrap();

        // Write a couple of bytes and verify them.
        let nr_bytes = 1000;
        assert_eq!(dt.write(0, nr_bytes).unwrap(), nr_bytes);
        assert_eq!(dt.verify(0, std::u64::MAX).unwrap(), nr_bytes);

        // Write a couple of bytes and verify half of them.
        let nr_bytes = 1000;
        loc_file.set_len(0).unwrap();
        assert_eq!(dt.write(0, nr_bytes).unwrap(), nr_bytes);
        assert_eq!(dt.verify(0, nr_bytes / 2).unwrap(), nr_bytes / 2);

        // Write a big chunk that is aggregated and verify it.
        loc_file.set_len(0).unwrap();
        let nr_bytes = (DtStream::CHUNKSIZE * nr_threads * 2 + 100) as u64;
        assert_eq!(dt.write(0, nr_bytes).unwrap(), nr_bytes);
        assert_eq!(dt.verify(0, std::u64::MAX).unwrap(), nr_bytes);

        // Check whether write rewinds the file.
        let nr_bytes = 1000;
        loc_file.set_len(100).unwrap();
        loc_file.seek(SeekFrom::Start(10)).unwrap();
        assert_eq!(dt.write(0, nr_bytes).unwrap(), nr_bytes);
        assert_eq!(dt.verify(0, std::u64::MAX).unwrap(), nr_bytes);

        // Modify the written data and assert failure.
        let nr_bytes = 1000;
        loc_file.set_len(0).unwrap();
        assert_eq!(dt.write(0, nr_bytes).unwrap(), nr_bytes);
        loc_file.seek(SeekFrom::Start(10)).unwrap();
        writeln!(loc_file, "x").unwrap();
        match dt.verify(0, nr_bytes) {
            Ok(_) => panic!("Verify of modified data did not fail!"),
            Err(e) => assert_eq!(e.to_string(), "Data MISMATCH at Byte 10!"),
        }
    }
}

// vim: ts=4 sw=4 expandtab
