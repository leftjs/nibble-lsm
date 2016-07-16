use common::*;
use segment;
use segment::*;
use epoch::*;
use memory::*;

use std::cmp;
use std::mem::size_of;
use std::ptr;
use std::sync::Arc;

use rand::{self,Rng};
use parking_lot as pl;

/// Acquire read lock on SegmentRef
macro_rules! rlock {
    ( $segref:expr ) => {
        $segref.unwrap().read().unwrap()
    }
}

/// Acquire write lock on SegmentRef
macro_rules! wlock {
    ( $segref:expr ) => {
        $segref.unwrap().write().unwrap()
    }
}

//==----------------------------------------------------==//
//      Constants
//==----------------------------------------------------==//

pub const NUM_LOG_HEADS: u32 = 1;

//==----------------------------------------------------==//
//      Entry header
//==----------------------------------------------------==//

/// Describe entry in the log. Format is:
///     | EntryHeader | Key bytes | Data bytes |
/// This struct MUST NOT contain any pointers.
#[derive(Debug)]
#[repr(C)]
pub struct EntryHeader {
    keylen: u32,
    datalen: u32,
}

// TODO can I get rid of most of this?
// e.g. use std::ptr::read / write instead?
impl EntryHeader {

    pub fn new(desc: &ObjDesc) -> Self {
        assert!(desc.keylen() <= usize::max_value());
        assert!(!desc.getvalue().0 .is_null());
        EntryHeader {
            keylen: desc.keylen() as u32,
            datalen: desc.valuelen(),
        }
    }

    pub fn empty() -> Self {
        EntryHeader {
            keylen: 0 as u32,
            datalen: 0 as u32,
        }
    }

    pub fn getdatalen(&self) -> u32 { self.datalen }
    pub fn getkeylen(&self) -> u32 { self.keylen }
    pub fn object_length(&self) -> u32 { self.datalen + self.keylen }
    pub fn len_with_header(&self) -> usize {
        (self.object_length() as usize) + size_of::<EntryHeader>()
    }

    /// Size of this (entire) entry in the log.
    pub fn len(&self) -> usize {
        size_of::<EntryHeader>() +
            self.keylen as usize +
            self.datalen as usize
    }

    pub fn as_ptr(&self) -> *const u8 {
        self as *const Self as *const u8
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self as *mut Self as *mut u8
    }

    /// Give the starting address of the object in the log, provided
    /// the address of this EntryHeader within the log. Implementation
    /// is non-trivial, as we must consider the non-contiguity of
    /// blocks.
    pub fn data_address(&self, entry: usize) -> *const u8 {
        unimplemented!();
    }

    #[cfg(test)]
    pub fn set_key_len(&mut self, l: u32) { self.keylen = l; }

    #[cfg(test)]
    pub fn set_data_len(&mut self, l: u32) { self.datalen = l; }
}


//==----------------------------------------------------==//
//      Log head
//==----------------------------------------------------==//

pub type LogHeadRef = Arc<pl::Mutex<LogHead>>;

macro_rules! loghead_ref {
    ( $manager:expr ) => {
        Arc::new( pl::Mutex::new(
                LogHead::new($manager)
                ))
    }
}

pub struct LogHead {
    segment: Option<SegmentRef>,
    manager: SegmentManagerRef,
}

// TODO when head is rolled, don't want to contend with other threads
// when handing it off to the compactor. we could keep a 'closed
// segment pool' with each log head. then periodically merge them into
// the compactor. this pool could be a concurrent queue with atomic
// push/pop. for now we just shove it into the compactor directly.

impl LogHead {

    pub fn new(manager: SegmentManagerRef) -> Self {
        LogHead { segment: None, manager: manager }
    }

    pub fn append(&mut self, buf: &ObjDesc) -> Status {
        assert!(buf.len_with_header() <
                (SEGMENT_SIZE-size_of::<SegmentHeader>()),
                "object {} larger than segment {}",
                buf.len_with_header(), SEGMENT_SIZE);

        let roll: bool;

        // check if head exists
        if let None = self.segment {
            trace!("head doesn't exist");
            roll = true;
        }
        // check if the object can fit in remaining space
        else {
            let segref = self.segment.clone().unwrap();
            roll = {
                let seg = segref.read();
                !seg.can_hold(buf)
            };
            if roll {
                debug!("rolling: head cannot hold new object");
            }
        }
        if roll {
            let socket = self.manager.socket();
            trace!("rolling head, socket {:?}", socket);
            if let Err(code) = self.roll() {
                return Err(code);
            }
        }

        // XXX clone then lock.. yuck
        let segref = self.segment.clone().unwrap();
        let mut seg = segref.write();
        match seg.append(buf) {
            Err(s) => panic!("has space but append failed: {:?}",s),
            va @ Ok(_) => va,
        }
    }

    //
    // --- Private methods ---
    //

    /// Replace the head segment.
    fn replace(&mut self) -> Status {
        self.segment = self.manager.alloc();
        match self.segment {
            None => Err(ErrorCode::OutOfMemory),
            _ => Ok(1),
        }
    }

    /// Upon closing a head segment, add reference to the recently
    /// closed list for the compaction code to pick up.
    /// TODO move to local head-specific pool to avoid locking
    fn add_closed(&mut self) {
        if let Some(segref) = self.segment.clone() {
            self.manager.add_closed(&segref);
        }
    }

    /// Roll head. Close current and allocate new.
    fn roll(&mut self) -> Status {
        let segref = self.segment.clone();
        if let Some(seg) = segref {
            seg.write().close();
            self.add_closed();
        }
        self.replace()
    }

}

//==----------------------------------------------------==//
//      The log
//==----------------------------------------------------==//

pub struct Log {
    heads: Vec<LogHeadRef>,
    manager: SegmentManagerRef,
    seginfo: SegmentInfoTableRef,
    // TODO track current capacity?
}

impl Log {

    pub fn new(manager: SegmentManagerRef) -> Self {
        let seginfo = manager.seginfo();
        let mut heads: Vec<LogHeadRef>;
        heads = Vec::with_capacity(NUM_LOG_HEADS as usize);
        for _ in 0..NUM_LOG_HEADS {
            heads.push(loghead_ref!(manager.clone()));
        }
        Log {
            heads: heads,
            manager: manager.clone(),
            seginfo: seginfo,
        }
    }

    /// Append an object to the log. If successful, returns the
    /// virtual address within the log inside Ok().
    /// FIXME check key is valid UTF-8
    pub fn append(&self, buf: &ObjDesc) -> Status {
        // 1. pick a log head XXX
        let x = unsafe { rdrand() } % NUM_LOG_HEADS;
        let head = &self.heads[x as usize];
        // 2. call append on the log head
        let va: usize = match head.lock().append(buf) {
            e @ Err(_) => return e,
            Ok(va) => va,
        };
        // 3. update segment info table
        // FIXME shouldn't have to lock for this
        let idx = self.manager.segment_of(va);
        let len = buf.len_with_header();
        debug_assert!(len < SEGMENT_SIZE);
        self.seginfo.incr_live(idx, len);
        // 4. return virtual address of new object
        Ok(va)
    }

    /// Pull out the value for an entry within the log (not the entire
    /// object).
    pub fn get_entry(&self, va: usize) -> Buffer {
        let block: Block = self.manager.block_of(va);
        debug_assert_eq!(block.list().ptr().is_null(), false);
        let usl = block.list();
        debug_assert!(block.blk_idx() < usl.len(),
            "block idx {} out of bounds for uslice {}",
            block.blk_idx(), usl.len());
        let list: &[BlockRef] = unsafe { usl.slice() };
        let entry = get_ref(list, block.blk_idx(), va);
        let mut buf = Buffer::new(entry.datalen as usize);
        unsafe { entry.get_data(buf.as_mut_ptr()); }
        buf
    }

    //
    // --- Internal methods used for testing only ---
    //

    #[cfg(test)]
    pub fn seginfo(&self) -> SegmentInfoTableRef { self.seginfo.clone() }
}

//==----------------------------------------------------==//
//      Entry reference
//==----------------------------------------------------==//

/// Reference to entry in the log. Used by Segment iterators since i)
/// items in memory don't have an associated language type (this
/// provides that function) and ii) we want to avoid copying objects
/// each time a reference is passed around; we lazily copy the object
/// from the log only when a client asks for it
#[derive(Debug)]
pub struct EntryReference<'a> {
    pub offset: usize, // into first block
    pub len: usize, /// header + key + data
    pub keylen: u32,
    pub datalen: u32,
    /// TODO can we avoid cloning the Arcs?
    pub blocks: &'a [BlockRef]
}

// TODO optimize for cases where the blocks are contiguous
// copying directly, or avoid copying (provide reference to it)
impl<'a> EntryReference<'a> {

    pub fn get_loc(&self) -> usize {
        self.offset + self.blocks[0].addr()
    }

    /// Copy out the key
    pub unsafe fn get_key(&self) -> u64 {
        let mut offset = self.offset + size_of::<EntryHeader>();
        let mut key: u64 = 0;
        // TODO optimize if contiguous
        // hm, lots of overhead for copying 8 bytes
        segment::copy_out(&self.blocks, offset,
                          &mut key as *mut u64 as *mut u8,
                          size_of::<u64>());
        key
    }

    /// Copy out the value
    pub unsafe fn get_data(&self, out: *mut u8) {
        let mut offset = self.offset + self.len
                            - self.datalen as usize;
        // TODO optimize if contiguous
        segment::copy_out(&self.blocks, offset,
                          out, self.datalen as usize);
    }

}

/// Construct an EntryReference given a VA and a set of Blocks.
pub fn get_ref(list: &[BlockRef], idx: usize, va: usize) -> EntryReference {
    let mut header: EntryHeader;
    let mut href: &EntryHeader;
    let offset = va & BLOCK_OFF_MASK;
    let blk_tail = BLOCK_SIZE - offset;
    let len = size_of::<EntryHeader>();

    if blk_tail >= len {
        let head_addr = va as *const usize as *const EntryHeader;
        href = unsafe { &*head_addr };
    } else { unsafe {
        header = EntryHeader::empty();
        copy_out(&list[idx..], offset, header.as_mut_ptr(), len);
        href = &header;
    }}

    debug_assert_eq!(href.getkeylen() as usize, size_of::<u64>());
    debug_assert!(href.getdatalen() > 0);
    // https://github.com/rust-lang/rust/issues/22644
    debug_assert!( (href.getdatalen() as usize) < SEGMENT_SIZE);

    // determine which blocks belong
    let mut nblks = 1;
    let entry_len = href.len_with_header();
    if entry_len > blk_tail {
        nblks += ((entry_len - blk_tail) / BLOCK_SIZE) + 1;
    }
    debug_assert!( (idx + nblks - 1) < list.len() );

    EntryReference {
        offset: offset,
        len: entry_len,
        keylen: href.getkeylen(),
        datalen: href.getdatalen(),
        blocks: &list[idx..(idx + nblks)],
    }
}

//==----------------------------------------------------==//
//      Unit tests
//==----------------------------------------------------==//

#[cfg(IGNORE)]
mod tests {
    use super::*;

    use std::ptr;
    use std::sync::{Arc,Mutex};

    use segment::*;
    use common::*;

    use super::super::logger;

    #[test]
    fn log_alloc_until_full() {
        logger::enable();
        let memlen = 1<<27;
        let manager = segmgr_ref!(SEGMENT_SIZE, memlen);
        let log = Log::new(manager);
        let key = String::from("keykeykeykey");
        let mut val = String::from("valuevaluevalue");
        for _ in 0..200 {
            val.push_str("valuevaluevaluevaluevalue");
        }
        let obj = ObjDesc::new2(&key, &val);
        loop {
            if let Err(code) = log.append(&obj) {
                match code {
                    ErrorCode::OutOfMemory => break,
                    _ => panic!("filling log returned {:?}", code),
                }
            }
        } // loop
    }

    // TODO fill log 50%, delete random items, then manually force
    // cleaning to test it

    // FIXME rewrite these unit tests

    #[test]
    fn entry_header_readwrite() {
        logger::enable();
        // get some raw memory
        let mem: Box<[u8;32]> = Box::new([0 as u8; 32]);
        let ptr = Box::into_raw(mem);

        // put a header into it with known values
        let mut header = EntryHeader::empty();
        header.set_key_len(5);
        header.set_data_len(7);
        assert_eq!(header.getkeylen(), 5);
        assert_eq!(header.getdatalen(), 7);

        unsafe {
            ptr::write(ptr as *mut EntryHeader, header);
        }

        // reset our copy, and re-read from raw memory
        unsafe {
            header = ptr::read(ptr as *const EntryHeader);
        }
        assert_eq!(header.getkeylen(), 5);
        assert_eq!(header.getdatalen(), 7);

        // free the original memory again
        unsafe { Box::from_raw(ptr); }
    }
}
