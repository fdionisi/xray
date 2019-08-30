use parking_lot::Mutex;
use std::fs;
use std::io;
use std::sync::Arc;

pub struct File {
    // id: xray_fs::FileId,
    file: Arc<Mutex<fs::File>>,
}

impl File {
    fn new(file: fs::File) -> Result<File, io::Error> {
        Ok(File {
            // id: file.metadata()?.ino(),
            file: Arc::new(Mutex::new(file)),
        })
    }
}

// impl xray_fs::File for File {
//     fn id(&self) -> xray_fs::FileId {
//         self.id
//     }
//
//     fn read(&self) -> Box<Future<Item = String, Error = io::Error>> {
//         let (tx, rx) = futures::sync::oneshot::channel();
//         let file = self.file.clone();
//         thread::spawn(move || {
//             fn read(file: &fs::File) -> Result<String, io::Error> {
//                 let mut buf_reader = io::BufReader::new(file);
//                 let mut contents = String::new();
//                 buf_reader.read_to_string(&mut contents)?;
//                 Ok(contents)
//             }
//
//             let _ = tx.send(read(&file.lock()));
//         });
//
//         Box::new(rx.then(|result| result.expect("Sender should not be dropped")))
//     }
//
//     fn write_snapshot(
//         &self,
//         snapshot: BufferSnapshot,
//     ) -> Box<Future<Item = (), Error = io::Error>> {
//         let (tx, rx) = futures::sync::oneshot::channel();
//         let file = self.file.clone();
//         thread::spawn(move || {
//             fn write(file: &mut fs::File, snapshot: BufferSnapshot) -> Result<(), io::Error> {
//                 let mut size = 0_u64;
//                 {
//                     let mut buf_writer = io::BufWriter::new(&mut *file);
//                     buf_writer.seek(SeekFrom::Start(0))?;
//                     for character in snapshot
//                         .iter()
//                         .flat_map(|c| decode_utf16(c.iter().cloned()))
//                     {
//                         let character = character.map_err(|_| {
//                             io::Error::new(
//                                 io::ErrorKind::InvalidData,
//                                 "buffer did not contain valid UTF-8",
//                             )
//                         })?;
//                         let mut encode_buf = [0_u8; 4];
//                         let encoded_char = character.encode_utf8(&mut encode_buf);
//                         buf_writer.write(encoded_char.as_bytes())?;
//                         size += encoded_char.len() as u64;
//                     }
//                 }
//                 file.set_len(size)?;
//                 Ok(())
//             }
//
//             let _ = tx.send(write(&mut file.lock(), snapshot));
//         });
//         Box::new(rx.then(|result| result.expect("Sender should not be dropped")))
//     }
// }
