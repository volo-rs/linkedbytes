//! [`LinkedBytes`] is a linked list of [`Bytes`] and [`BytesMut`] (though we use VecDeque to
//! implement it now).
//!
//! It is primarily used to manage [`Bytes`] and [`BytesMut`] and make a [`&[IoSlice<'_>]`]
//! to be used by `writev`.
use std::{collections::VecDeque, io::IoSlice};

use bytes::{BufMut, Bytes, BytesMut};

const DEFAULT_BUFFER_SIZE: usize = 8192; // 8KB
const DEFAULT_DEQUE_SIZE: usize = 16;

pub struct LinkedBytes {
    // This is used to avoid allocating a new Vec when calling `as_ioslice`.
    // It is self-referential in fact, but we can guarantee that it is safe,
    // so we just use `'static` here.
    // [`ioslice`] must be the first field, so that it is dropped before [`list`]
    // and [`bytes`] to keep soundness.
    ioslice: Vec<IoSlice<'static>>,

    bytes: BytesMut,
    list: VecDeque<Node>,
}

pub enum Node {
    Bytes(Bytes),
    BytesMut(BytesMut),
}

impl AsRef<[u8]> for Node {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Node::Bytes(b) => b.as_ref(),
            Node::BytesMut(b) => b.as_ref(),
        }
    }
}

impl LinkedBytes {
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_SIZE)
    }

    pub fn with_capacity(cap: usize) -> Self {
        let bytes = BytesMut::with_capacity(cap);
        let list = VecDeque::with_capacity(DEFAULT_DEQUE_SIZE);
        Self {
            list,
            bytes,
            ioslice: Vec::with_capacity(DEFAULT_DEQUE_SIZE),
        }
    }

    #[inline]
    pub fn bytes(&self) -> &BytesMut {
        &self.bytes
    }

    #[inline]
    pub fn bytes_mut(&mut self) -> &mut BytesMut {
        &mut self.bytes
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    pub fn insert(&mut self, bytes: Bytes) {
        let node = Node::Bytes(bytes);
        // split current bytes
        let prev = self.bytes.split();

        self.list.push_back(Node::BytesMut(prev));
        self.list.push_back(node);
    }

    pub fn as_ioslice(&self) -> Vec<IoSlice<'_>> {
        let mut ioslice = Vec::with_capacity(self.list.len());
        for node in self.list.iter() {
            match node {
                Node::Bytes(bytes) => ioslice.push(IoSlice::new(bytes.as_ref())),
                Node::BytesMut(bytes) => ioslice.push(IoSlice::new(bytes.as_ref())),
            }
        }
        ioslice.push(IoSlice::new(self.bytes.as_ref()));
        ioslice
    }

    // #[allow(clippy::needless_lifetimes)]
    // pub fn as_ioslice<'a>(&'a mut self) -> &'a [IoSlice<'a>] {
    //     self.ioslice.reserve(self.list.len() + 1);
    //     for node in self.list.iter() {
    //         match node {
    //             // Safety: we will change this back to `'a` later.
    //             Node::Bytes(bytes) => self
    //                 .ioslice
    //                 .push(IoSlice::new(unsafe { &*(bytes.as_ref() as *const _) })),
    //             Node::BytesMut(bytes) => self
    //                 .ioslice
    //                 .push(IoSlice::new(unsafe { &*(bytes.as_ref() as *const _) })),
    //         }
    //     }
    //     // don't forget to push self.bytes
    //     self.ioslice
    //         .push(IoSlice::new(unsafe { &*(self.bytes.as_ref() as *const _) }));
    //     // Safety: we can guarantee that the returned `&[IoSlice<'_>]`'s lifetime can't
    //     // outlive self.
    //     unsafe { &*(self.ioslice.as_mut_slice() as *mut _) }
    // }

    pub fn reset(&mut self) {
        // ioslice must be cleared before list
        self.ioslice.clear();

        if self.list.is_empty() {
            // only clear bytes
            self.bytes.clear();
            return;
        }

        let Node::BytesMut(mut head) = self.list.pop_front().unwrap() else {
            // this should not happen
            panic!("head is not BytesMut");
        };

        while let Some(node) = self.list.pop_front() {
            if let Node::BytesMut(next_buf) = node {
                head.unsplit(next_buf);
            }
        }

        // don't forget to unsplit self.bytes
        // here we need to do this in a tricky way, because we can't move self.bytes
        unsafe {
            self.bytes.set_len(self.bytes.capacity());
        }
        let remaining = self.bytes.split();
        head.unsplit(remaining);
        self.bytes = head;

        self.bytes.clear();
    }
}

impl Default for LinkedBytes {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl BufMut for LinkedBytes {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.bytes.remaining_mut()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        self.bytes.advance_mut(cnt)
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        self.bytes.chunk_mut()
    }
}
