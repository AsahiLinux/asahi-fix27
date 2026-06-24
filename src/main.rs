// SPDX-License-Identifier: GPL-2.0-only

use anyhow::Result;
use gpt::{GptConfig, disk::LogicalBlockSize};
use std::{
    env,
    fs::File,
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
};
use uuid::Uuid;

struct NxSuperblock([u8; NxSuperblock::SIZE]);

impl NxSuperblock {
    const SIZE: usize = 1408;
    const MAGIC: u32 = 1112758350; //'BSXN'
    const MAX_FILE_SYSTEMS: usize = 100;
    fn get_buf(&mut self) -> &mut [u8] {
        &mut self.0
    }
    fn new() -> Self {
        NxSuperblock([0; NxSuperblock::SIZE])
    }
    fn magic(&self) -> u32 {
        u32::from_le_bytes(self.0[32..32 + 4].try_into().unwrap())
    }
    fn block_size(&self) -> u32 {
        u32::from_le_bytes(self.0[36..36 + 4].try_into().unwrap())
    }
    fn xid(&self) -> u64 {
        u64::from_le_bytes(self.0[16..16 + 8].try_into().unwrap())
    }
    fn omap_oid(&self) -> u64 {
        u64::from_le_bytes(self.0[160..160 + 8].try_into().unwrap())
    }
    fn xp_desc_blocks(&self) -> u32 {
        u32::from_le_bytes(self.0[104..104 + 4].try_into().unwrap())
    }
    fn xp_desc_base(&self) -> u64 {
        u64::from_le_bytes(self.0[112..112 + 8].try_into().unwrap())
    }
    fn fs_oid(&self, i: usize) -> u64 {
        let at = 184 + 8 * i;
        u64::from_le_bytes(self.0[at..at + 8].try_into().unwrap())
    }
}

struct OmapPhys<'a>(&'a [u8]);
impl OmapPhys<'_> {
    const SIZE: usize = 88;
    fn tree_oid(&self) -> u64 {
        u64::from_le_bytes(self.0[48..48 + 8].try_into().unwrap())
    }
}

struct NLoc<'a>(&'a [u8]);

impl NLoc<'_> {
    fn off(&self) -> u16 {
        u16::from_le_bytes(self.0[0..2].try_into().unwrap())
    }
    fn len(&self) -> u16 {
        u16::from_le_bytes(self.0[2..2 + 2].try_into().unwrap())
    }
}

struct KVOff<'a>(&'a [u8]);
impl KVOff<'_> {
    const SIZE: usize = 4;
    fn k(&self) -> u16 {
        u16::from_le_bytes(self.0[0..2].try_into().unwrap())
    }
    fn v(&self) -> u16 {
        u16::from_le_bytes(self.0[2..2 + 2].try_into().unwrap())
    }
}

struct OmapKey<'a>(&'a [u8]);
impl OmapKey<'_> {
    fn oid(&self) -> u64 {
        u64::from_le_bytes(self.0[0..8].try_into().unwrap())
    }
}

struct OmapVal<'a>(&'a [u8]);
impl OmapVal<'_> {
    fn paddr(&self) -> u64 {
        u64::from_le_bytes(self.0[8..8 + 8].try_into().unwrap())
    }
}

struct BTreeInfo;
impl BTreeInfo {
    const SIZE: usize = 40;
}

struct BTreeNodePhys<'a>(&'a [u8]);
impl BTreeNodePhys<'_> {
    const FIXED_KV_SIZE: u16 = 0x4;
    const ROOT: u16 = 0x1;
    const SIZE: usize = 56;
    fn flags(&self) -> u16 {
        u16::from_le_bytes(self.0[32..32 + 2].try_into().unwrap())
    }
    fn level(&self) -> u16 {
        u16::from_le_bytes(self.0[34..34 + 2].try_into().unwrap())
    }
    fn table_space(&self) -> NLoc<'_> {
        NLoc(&self.0[40..])
    }
    fn nkeys(&self) -> u32 {
        u32::from_le_bytes(self.0[36..36 + 4].try_into().unwrap())
    }
}

const APFS_FS_FLAGS_OFFSET: usize = 264;
const OBJ_PHYS_CKSUM_OFFSET: usize = 0;
struct ApfsSuperblock<'a>(&'a [u8]);
impl ApfsSuperblock<'_> {
    fn volname(&self) -> &[u8] {
        &self.0[704..704 + 128]
    }
    fn volume_group_id(&self) -> Uuid {
        Uuid::from_slice(&self.0[1008..1008 + 16]).unwrap()
    }
    fn role(&self) -> u16 {
        u16::from_le_bytes(self.0[964..964 + 2].try_into().unwrap())
    }
    fn fs_flags(&self) -> u64 {
        u64::from_le_bytes(
            self.0[APFS_FS_FLAGS_OFFSET..APFS_FS_FLAGS_OFFSET + 8]
                .try_into()
                .unwrap(),
        )
    }
}

const VOL_ROLE_SYSTEM: u16 = 1;
const APFS_FLAG_VOL_BOOTABLE: u64 = 0x200;

fn pread<T: Read + Seek>(file: &mut T, pos: u64, target: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(pos))?;
    file.read_exact(target)
}

fn pwrite<T: Write + Seek>(file: &mut T, pos: u64, data: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(pos))?;
    file.write_all(data)
}

// should probably fix xids here
fn lookup(_disk: &mut File, cur_node: &BTreeNodePhys, key: u64) -> Option<u64> {
    if cur_node.level() != 0 {
        unimplemented!();
    }
    if cur_node.flags() & BTreeNodePhys::FIXED_KV_SIZE != 0 {
        let toc_off = cur_node.table_space().off() as usize + BTreeNodePhys::SIZE;
        let key_start = toc_off + cur_node.table_space().len() as usize;
        let val_end = cur_node.0.len()
            - if cur_node.flags() & BTreeNodePhys::ROOT == 0 {
                0
            } else {
                BTreeInfo::SIZE
            };
        for i in 0..cur_node.nkeys() as usize {
            let entry = KVOff(&cur_node.0[(toc_off + i * KVOff::SIZE)..]);
            let key_off = entry.k() as usize + key_start;
            let map_key = OmapKey(&cur_node.0[key_off..]);
            if map_key.oid() == key {
                let val_off = val_end - entry.v() as usize;
                let val = OmapVal(&cur_node.0[val_off..]);
                return Some(val.paddr());
            }
        }
        None
    } else {
        unimplemented!();
    }
}

fn trim_zeroes(s: &[u8]) -> &[u8] {
    for i in 0..s.len() {
        if s[i] == 0 {
            return &s[..i];
        }
    }
    s
}

fn fletcher(data: &[u8]) -> u64 {
    let mut s1 = 0;
    let mut s2 = 0;
    for ch in data.chunks(4) {
        let u = u32::from_le_bytes(ch.try_into().unwrap()) as u64;
        s1 += u;
        s2 += s1;
    }
    let c1 = 0xFFFFFFFF - (s1 + s2) % 0xFFFFFFFF;
    let c2 = 0xFFFFFFFF - (s1 + c1) % 0xFFFFFFFF;
    (c2 << 32) | c1
}

fn scan_volume(disk: &mut File, proceed: bool) -> Result<bool> {
    let mut sb = NxSuperblock::new();
    disk.read_exact(sb.get_buf())?;
    if sb.magic() != NxSuperblock::MAGIC {
        return Ok(false);
    }
    let block_size = sb.block_size() as u64;
    for i in 0..sb.xp_desc_blocks() {
        let mut sbc = NxSuperblock::new();
        pread(
            disk,
            (sb.xp_desc_base() + i as u64) * block_size,
            sbc.get_buf(),
        )?;
        if sbc.magic() == NxSuperblock::MAGIC && sbc.xid() > sb.xid() {
            sb = sbc;
        }
    }
    let mut omap_bytes = vec![0; OmapPhys::SIZE];
    pread(disk, sb.omap_oid() * block_size, &mut omap_bytes)?;
    let omap = OmapPhys(&omap_bytes);
    let mut node_bytes = vec![0; sb.block_size() as usize];
    pread(disk, omap.tree_oid() * block_size, &mut node_bytes)?;
    let node = BTreeNodePhys(&node_bytes);
    let mut found_any = false;
    for i in 0..NxSuperblock::MAX_FILE_SYSTEMS {
        let fs_id = sb.fs_oid(i);
        if fs_id == 0 {
            continue;
        }
        let vsb = lookup(disk, &node, fs_id);
        let mut asb_bytes = vec![0; sb.block_size() as usize];
        if vsb.is_none() {
            continue;
        }
        let asb_offset = vsb.unwrap() * sb.block_size() as u64;
        pread(disk, asb_offset, &mut asb_bytes)?;
        let asb = ApfsSuperblock(&asb_bytes);
        if asb.volume_group_id().is_nil() {
            continue;
        }
        if asb.role() != VOL_ROLE_SYSTEM {
            continue;
        }
        let name = String::from_utf8_lossy(trim_zeroes(asb.volname()));
        if asb.fs_flags() & APFS_FLAG_VOL_BOOTABLE == 0 {
            let msg = if proceed { "Setting" } else { "Will set" };
            println!("{} volume \"{}\" bootable", msg, name);
            let nflags = (asb.fs_flags() | APFS_FLAG_VOL_BOOTABLE).to_le_bytes();
            asb_bytes[APFS_FS_FLAGS_OFFSET..APFS_FS_FLAGS_OFFSET + 8].copy_from_slice(&nflags);
            if proceed {
                pwrite(disk, asb_offset + APFS_FS_FLAGS_OFFSET as u64, &nflags)?;
                pwrite(
                    disk,
                    asb_offset + OBJ_PHYS_CKSUM_OFFSET as u64,
                    &fletcher(&asb_bytes[8..]).to_le_bytes(),
                )?;
            }
            found_any = true;
        }
    }
    Ok(found_any)
}

fn scan_disks(proceed: bool) -> Result<bool> {
    let disk = GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb4096)
        .open("/dev/nvme0n1");
    let disk = match disk {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("Unable to open the disk, try running with sudo?");
            return Ok(false);
        }
        e => e?,
    };
    let mut found_any = false;
    for (i, v) in disk.partitions() {
        if v.part_type_guid.guid != "7C3457EF-0000-11AA-AA11-00306543ECAC" {
            continue;
        }
        let mut part = File::options()
            .read(true)
            .write(proceed)
            .open(format!("/dev/nvme0n1p{i}"))?;
        found_any |= scan_volume(&mut part, proceed)?;
    }
    Ok(found_any)
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.len() >= 3 || (args.len() == 2 && args[1] != "--confirm") {
        println!("Usage: {} [--confirm]", args[0]);
        println!("\t--confirm\twrite the changes to disk");
        return;
    }
    let proceed = args.len() == 2 && args[1] == "--confirm";
    let found = scan_disks(proceed).unwrap();
    if !proceed && found {
        println!("Run `{} --confirm` to apply the changes", args[0]);
    }
}
