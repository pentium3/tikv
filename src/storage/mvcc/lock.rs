// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use super::super::types::Value;
use super::{Error, Result, TsSet};
use crate::storage::{
    Key, Mutation, FOR_UPDATE_TS_PREFIX, MIN_COMMIT_TS_PREFIX, SHORT_VALUE_MAX_LEN,
    SHORT_VALUE_PREFIX, TXN_SIZE_PREFIX,
};
use byteorder::ReadBytesExt;
use kvproto::kvrpcpb::{LockInfo, Op};
use tikv_util::codec::bytes::{self, BytesEncoder};
use tikv_util::codec::number::{self, NumberEncoder, MAX_VAR_U64_LEN};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockType {
    Put,
    Delete,
    Lock,
    Pessimistic,
}

const FLAG_PUT: u8 = b'P';
const FLAG_DELETE: u8 = b'D';
const FLAG_LOCK: u8 = b'L';
const FLAG_PESSIMISTIC: u8 = b'S';

impl LockType {
    pub fn from_mutation(mutation: &Mutation) -> LockType {
        match *mutation {
            Mutation::Put(_) | Mutation::Insert(_) => LockType::Put,
            Mutation::Delete(_) => LockType::Delete,
            Mutation::Lock(_) => LockType::Lock,
        }
    }

    fn from_u8(b: u8) -> Option<LockType> {
        match b {
            FLAG_PUT => Some(LockType::Put),
            FLAG_DELETE => Some(LockType::Delete),
            FLAG_LOCK => Some(LockType::Lock),
            FLAG_PESSIMISTIC => Some(LockType::Pessimistic),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            LockType::Put => FLAG_PUT,
            LockType::Delete => FLAG_DELETE,
            LockType::Lock => FLAG_LOCK,
            LockType::Pessimistic => FLAG_PESSIMISTIC,
        }
    }
}

#[derive(PartialEq, Clone, Debug)]
pub struct Lock {
    pub lock_type: LockType,
    pub primary: Vec<u8>,
    pub ts: u64,
    pub ttl: u64,
    pub short_value: Option<Value>,
    // If for_update_ts != 0, this lock belongs to a pessimistic transaction
    pub for_update_ts: u64,
    pub txn_size: u64,
    pub min_commit_ts: u64,
}

impl Lock {
    pub fn new(
        lock_type: LockType,
        primary: Vec<u8>,
        ts: u64,
        ttl: u64,
        short_value: Option<Value>,
        for_update_ts: u64,
        txn_size: u64,
        min_commit_ts: u64,
    ) -> Lock {
        Lock {
            lock_type,
            primary,
            ts,
            ttl,
            short_value,
            for_update_ts,
            txn_size,
            min_commit_ts,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(
            1 + MAX_VAR_U64_LEN + self.primary.len() + MAX_VAR_U64_LEN + SHORT_VALUE_MAX_LEN + 2,
        );
        b.push(self.lock_type.to_u8());
        b.encode_compact_bytes(&self.primary).unwrap();
        b.encode_var_u64(self.ts).unwrap();
        b.encode_var_u64(self.ttl).unwrap();
        if let Some(ref v) = self.short_value {
            b.push(SHORT_VALUE_PREFIX);
            b.push(v.len() as u8);
            b.extend_from_slice(v);
        }
        if self.for_update_ts > 0 {
            b.push(FOR_UPDATE_TS_PREFIX);
            b.encode_u64(self.for_update_ts).unwrap();
        }
        if self.txn_size > 0 {
            b.push(TXN_SIZE_PREFIX);
            b.encode_u64(self.txn_size).unwrap();
        }
        if self.min_commit_ts > 0 {
            b.push(MIN_COMMIT_TS_PREFIX);
            b.encode_u64(self.min_commit_ts).unwrap();
        }
        b
    }

    pub fn parse(mut b: &[u8]) -> Result<Lock> {
        if b.is_empty() {
            return Err(Error::BadFormatLock);
        }
        let lock_type = LockType::from_u8(b.read_u8()?).ok_or(Error::BadFormatLock)?;
        let primary = bytes::decode_compact_bytes(&mut b)?;
        let ts = number::decode_var_u64(&mut b)?;
        let ttl = if b.is_empty() {
            0
        } else {
            number::decode_var_u64(&mut b)?
        };

        if b.is_empty() {
            return Ok(Lock::new(lock_type, primary, ts, ttl, None, 0, 0, 0));
        }

        let mut short_value = None;
        let mut for_update_ts = 0;
        let mut txn_size: u64 = 0;
        let mut min_commit_ts: u64 = 0;
        while !b.is_empty() {
            match b.read_u8()? {
                SHORT_VALUE_PREFIX => {
                    let len = b.read_u8()?;
                    if b.len() < len as usize {
                        panic!(
                            "content len [{}] shorter than short value len [{}]",
                            b.len(),
                            len,
                        );
                    }
                    short_value = Some(b[..len as usize].to_vec());
                    b = &b[len as usize..];
                }
                FOR_UPDATE_TS_PREFIX => for_update_ts = number::decode_u64(&mut b)?,
                TXN_SIZE_PREFIX => txn_size = number::decode_u64(&mut b)?,
                MIN_COMMIT_TS_PREFIX => min_commit_ts = number::decode_u64(&mut b)?,
                flag => panic!("invalid flag [{}] in lock", flag),
            }
        }
        Ok(Lock::new(
            lock_type,
            primary,
            ts,
            ttl,
            short_value,
            for_update_ts,
            txn_size,
            min_commit_ts,
        ))
    }

    pub fn into_lock_info(self, raw_key: Vec<u8>) -> LockInfo {
        let mut info = LockInfo::default();
        info.set_primary_lock(self.primary);
        info.set_lock_version(self.ts);
        info.set_key(raw_key);
        info.set_lock_ttl(self.ttl);
        info.set_txn_size(self.txn_size);
        let lock_type = match self.lock_type {
            LockType::Put => Op::Put,
            LockType::Delete => Op::Del,
            LockType::Lock => Op::Lock,
            LockType::Pessimistic => Op::PessimisticLock,
        };
        info.set_lock_type(lock_type);
        info
    }

    /// Checks whether the lock conflicts with the given `ts`. If `ts == MaxU64`, the primary lock will be ignored.
    pub fn check_ts_conflict(self, key: &Key, ts: u64, bypass_locks: &TsSet) -> Result<()> {
        if self.ts > ts
            || self.lock_type == LockType::Lock
            || self.lock_type == LockType::Pessimistic
        {
            // Ignore lock when lock.ts > ts or lock's type is Lock or Pessimistic
            return Ok(());
        }

        if bypass_locks.contains(self.ts) {
            return Ok(());
        }

        let raw_key = key.to_raw()?;

        if ts == std::u64::MAX && raw_key == self.primary {
            // When `ts == u64::MAX` (which means to get latest committed version for
            // primary key), and current key is the primary key, we ignore this lock.
            return Ok(());
        }

        // There is a pending lock. Client should wait or clean it.
        Err(Error::KeyIsLocked(self.into_lock_info(raw_key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Key, Mutation};

    #[test]
    fn test_lock_type() {
        let (key, value) = (b"key", b"value");
        let mut tests = vec![
            (
                Mutation::Put((Key::from_raw(key), value.to_vec())),
                LockType::Put,
                FLAG_PUT,
            ),
            (
                Mutation::Delete(Key::from_raw(key)),
                LockType::Delete,
                FLAG_DELETE,
            ),
            (
                Mutation::Lock(Key::from_raw(key)),
                LockType::Lock,
                FLAG_LOCK,
            ),
        ];
        for (i, (mutation, lock_type, flag)) in tests.drain(..).enumerate() {
            let lt = LockType::from_mutation(&mutation);
            assert_eq!(
                lt, lock_type,
                "#{}, expect from_mutation({:?}) returns {:?}, but got {:?}",
                i, mutation, lock_type, lt
            );
            let f = lock_type.to_u8();
            assert_eq!(
                f, flag,
                "#{}, expect {:?}.to_u8() returns {:?}, but got {:?}",
                i, lock_type, flag, f
            );
            let lt = LockType::from_u8(flag).unwrap();
            assert_eq!(
                lt, lock_type,
                "#{}, expect from_u8({:?}) returns {:?}, but got {:?})",
                i, flag, lock_type, lt
            );
        }
    }

    #[test]
    fn test_lock() {
        // Test `Lock::to_bytes()` and `Lock::parse()` works as a pair.
        let mut locks = vec![
            Lock::new(LockType::Put, b"pk".to_vec(), 1, 10, None, 0, 0, 0),
            Lock::new(
                LockType::Delete,
                b"pk".to_vec(),
                1,
                10,
                Some(b"short_value".to_vec()),
                0,
                0,
                0,
            ),
            Lock::new(LockType::Put, b"pk".to_vec(), 1, 10, None, 10, 0, 0),
            Lock::new(
                LockType::Delete,
                b"pk".to_vec(),
                1,
                10,
                Some(b"short_value".to_vec()),
                10,
                0,
                0,
            ),
            Lock::new(LockType::Put, b"pk".to_vec(), 1, 10, None, 0, 16, 0),
            Lock::new(
                LockType::Delete,
                b"pk".to_vec(),
                1,
                10,
                Some(b"short_value".to_vec()),
                0,
                16,
                0,
            ),
            Lock::new(LockType::Put, b"pk".to_vec(), 1, 10, None, 10, 16, 0),
            Lock::new(
                LockType::Delete,
                b"pk".to_vec(),
                1,
                10,
                Some(b"short_value".to_vec()),
                10,
                0,
                0,
            ),
            Lock::new(
                LockType::Put,
                b"pkpkpk".to_vec(),
                111,
                222,
                None,
                333,
                444,
                555,
            ),
        ];
        for (i, lock) in locks.drain(..).enumerate() {
            let v = lock.to_bytes();
            let l = Lock::parse(&v[..]).unwrap_or_else(|e| panic!("#{} parse() err: {:?}", i, e));
            assert_eq!(l, lock, "#{} expect {:?}, but got {:?}", i, lock, l);
        }

        // Test `Lock::parse()` handles incorrect input.
        assert!(Lock::parse(b"").is_err());

        let lock = Lock::new(
            LockType::Lock,
            b"pk".to_vec(),
            1,
            10,
            Some(b"short_value".to_vec()),
            0,
            0,
            0,
        );
        let v = lock.to_bytes();
        assert!(Lock::parse(&v[..4]).is_err());
    }

    #[test]
    fn test_check_ts_conflict() {
        let key = Key::from_raw(b"foo");
        let mut lock = Lock::new(LockType::Put, vec![], 100, 3, None, 0, 1, 0);

        let empty = Default::default();

        // Ignore the lock if read ts is less than the lock version
        lock.clone().check_ts_conflict(&key, 50, &empty).unwrap();

        // Returns the lock if read ts >= lock version
        lock.clone()
            .check_ts_conflict(&key, 110, &empty)
            .unwrap_err();

        // Ignore locks that occurs in the `bypass_locks` set.
        lock.clone()
            .check_ts_conflict(&key, 110, &TsSet::new(vec![109]))
            .unwrap_err();
        lock.clone()
            .check_ts_conflict(&key, 110, &TsSet::new(vec![110]))
            .unwrap_err();
        lock.clone()
            .check_ts_conflict(&key, 110, &TsSet::new(vec![100]))
            .unwrap();
        lock.clone()
            .check_ts_conflict(&key, 110, &TsSet::new(vec![99, 101, 102, 100, 80]))
            .unwrap();

        // Ignore the lock if it is Lock or Pessimistic.
        lock.lock_type = LockType::Lock;
        lock.clone().check_ts_conflict(&key, 110, &empty).unwrap();
        lock.lock_type = LockType::Pessimistic;
        lock.clone().check_ts_conflict(&key, 110, &empty).unwrap();

        // Ignore the primary lock when reading the latest committed version by setting u64::MAX as ts
        lock.lock_type = LockType::Put;
        lock.primary = b"foo".to_vec();
        lock.clone()
            .check_ts_conflict(&key, std::u64::MAX, &empty)
            .unwrap();

        // Should not ignore the secondary lock even though reading the latest version
        lock.primary = b"bar".to_vec();
        lock.clone()
            .check_ts_conflict(&key, std::u64::MAX, &empty)
            .unwrap_err();
    }
}
