//! [`LinkedBytes`] is a linked list of [`Bytes`] and [`BytesMut`] (though we use VecDeque to
//! implement it now).
//!
//! It is primarily used to manage [`Bytes`] and [`BytesMut`] and make a [`&[IoSlice<'_>]`]
//! to be used by `writev`.
use std::{collections::VecDeque, io::IoSlice};

use bytes::{BufMut, Bytes, BytesMut};
use faststr::FastStr;
use tokio::io::{AsyncWrite, AsyncWriteExt};

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
    FastStr(FastStr),
}

impl AsRef<[u8]> for Node {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Node::Bytes(b) => b.as_ref(),
            Node::BytesMut(b) => b.as_ref(),
            Node::FastStr(s) => s.as_ref(),
        }
    }
}

impl LinkedBytes {
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_SIZE)
    }

    #[inline]
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

    pub fn len(&self) -> usize {
        let mut len = 0;
        for node in self.list.iter() {
            len += node.as_ref().len();
        }
        len + self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn insert(&mut self, bytes: Bytes) {
        let node = Node::Bytes(bytes);
        // split current bytes
        let prev = self.bytes.split();

        self.list.push_back(Node::BytesMut(prev));
        self.list.push_back(node);
    }

    pub fn insert_faststr(&mut self, fast_str: FastStr) {
        let node = Node::FastStr(fast_str);
        // split current bytes
        let prev = self.bytes.split();

        self.list.push_back(Node::BytesMut(prev));
        self.list.push_back(node);
    }

    pub fn io_slice(&mut self) -> Vec<IoSlice<'_>> {
        let mut ioslice = Vec::with_capacity(self.list.len() + 1);
        for node in self.list.iter() {
            let bytes = node.as_ref();
            if bytes.is_empty() {
                continue;
            }
            ioslice.push(IoSlice::new(bytes));
        }
        ioslice.push(IoSlice::new(self.bytes.as_ref()));
        ioslice
    }

    // TODO: use write_all_vectored when stable
    pub async fn write_all_vectored<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        assert!(
            self.ioslice.is_empty(),
            "ioslice must be empty, maybe forget to call `reset`"
        );
        self.ioslice.reserve(self.list.len() + 1);
        // prepare ioslice
        for node in self.list.iter() {
            let bytes = node.as_ref();
            if bytes.is_empty() {
                continue;
            }
            // SAFETY: we can guarantee that the lifetime of `bytes` can't outlive self
            self.ioslice
                .push(IoSlice::new(unsafe { &*(bytes as *const _) }));
        }
        self.ioslice
            .push(IoSlice::new(unsafe { &*(self.bytes.as_ref() as *const _) }));

        // do write_all_vectored
        // we use usize here to avoid `Send` bound required for *mut IoSlice
        let (mut base_ptr, mut len) = (self.ioslice.as_mut_ptr() as usize, self.ioslice.len());
        while len != 0 {
            let ioslice = unsafe { std::slice::from_raw_parts(base_ptr as *mut IoSlice, len) };
            let n = writer.write_vectored(ioslice).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            // Number of buffers to remove.
            let mut remove = 0;
            // Total length of all the to be removed buffers.
            let mut accumulated_len = 0;
            for buf in ioslice.iter() {
                if accumulated_len + buf.len() > n {
                    break;
                } else {
                    accumulated_len += buf.len();
                    remove += 1;
                }
            }

            // adjust the outer [IoSlice]
            base_ptr = unsafe { (base_ptr as *mut IoSlice).add(remove) as usize };
            len -= remove;
            if len == 0 {
                assert!(
                    n == accumulated_len,
                    "advancing io slices beyond their length"
                );
            } else {
                // adjust the inner IoSlice
                let inner_slice = unsafe { &mut *(base_ptr as *mut IoSlice) };
                let (inner_ptr, inner_len) = (inner_slice.as_ptr(), inner_slice.len());
                let remaining = n - accumulated_len;
                assert!(
                    remaining <= inner_len,
                    "advancing io slice beyond its length"
                );
                let new_ptr = unsafe { inner_ptr.add(remaining) };
                let new_len = inner_len - remaining;
                *inner_slice =
                    IoSlice::new(unsafe { std::slice::from_raw_parts(new_ptr, new_len) });
            }
        }
        self.ioslice.clear();
        Ok(())
    }

    // TODO: use write_all_vectored when stable
    pub fn sync_write_all_vectored<W: std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        assert!(
            self.ioslice.is_empty(),
            "ioslice must be empty, maybe forget to call `reset`"
        );
        self.ioslice.reserve(self.list.len() + 1);
        // prepare ioslice
        for node in self.list.iter() {
            let bytes = node.as_ref();
            if bytes.is_empty() {
                continue;
            }
            // SAFETY: we can guarantee that the lifetime of `bytes` can't outlive self
            self.ioslice
                .push(IoSlice::new(unsafe { &*(bytes as *const _) }));
        }
        self.ioslice
            .push(IoSlice::new(unsafe { &*(self.bytes.as_ref() as *const _) }));

        // do write_all_vectored
        let (mut base_ptr, mut len) = (self.ioslice.as_mut_ptr(), self.ioslice.len());
        while len != 0 {
            let ioslice = unsafe { std::slice::from_raw_parts(base_ptr, len) };
            let n = writer.write_vectored(ioslice)?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            // Number of buffers to remove.
            let mut remove = 0;
            // Total length of all the to be removed buffers.
            let mut accumulated_len = 0;
            for buf in ioslice.iter() {
                if accumulated_len + buf.len() > n {
                    break;
                } else {
                    accumulated_len += buf.len();
                    remove += 1;
                }
            }

            // adjust the outer [IoSlice]
            base_ptr = unsafe { (base_ptr as *mut IoSlice).add(remove) };
            len -= remove;
            if len == 0 {
                assert!(
                    n == accumulated_len,
                    "advancing io slices beyond their length"
                );
            } else {
                // adjust the inner IoSlice
                let inner_slice = unsafe { &mut *(base_ptr as *mut IoSlice) };
                let (inner_ptr, inner_len) = (inner_slice.as_ptr(), inner_slice.len());
                let remaining = n - accumulated_len;
                assert!(
                    remaining <= inner_len,
                    "advancing io slice beyond its length"
                );
                let new_ptr = unsafe { inner_ptr.add(remaining) };
                let new_len = inner_len - remaining;
                *inner_slice =
                    IoSlice::new(unsafe { std::slice::from_raw_parts(new_ptr, new_len) });
            }
        }
        self.ioslice.clear();
        Ok(())
    }

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

// Unstable APIs
impl LinkedBytes {
    /// This splits the current bytes_mut and push it to the list.
    /// This is an unstable API that may change in the future, don't rely on this.
    /// Returns the index of the node.
    #[doc(hidden)]
    #[inline]
    pub fn split(&mut self) -> usize {
        let prev = self.bytes.split();
        let node = Node::BytesMut(prev);
        self.list.push_back(node);
        self.list.len() - 1
    }

    /// This gets the node at the given index.
    /// If you want to get the current bytes_mut, use `bytes_mut()` instead.
    /// This is an unstable API that may change in the future, don't rely on this.
    #[doc(hidden)]
    #[inline]
    pub fn get_list_mut(&mut self, index: usize) -> Option<&mut Node> {
        self.list.get_mut(index)
    }

    /// This gets the iterator of the list.
    /// This is an unstable API that may change in the future, don't rely on this.
    #[doc(hidden)]
    #[inline]
    pub fn iter_list(&self) -> impl Iterator<Item = &Node> {
        self.list.iter()
    }

    /// This converts the list to an iterator.
    /// This is an unstable API that may change in the future, don't rely on this.
    #[doc(hidden)]
    #[inline]
    pub fn into_iter_list(mut self) -> impl Iterator<Item = Node> {
        let node = Node::BytesMut(self.bytes);
        self.list.push_back(node);
        self.list.into_iter()
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
