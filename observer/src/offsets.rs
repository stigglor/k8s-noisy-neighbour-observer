//! Struct member offsets for the eBPF programs, from the BTF of the running kernel. See "Kernel
//! struct offsets" in `CLAUDE.md` for the reason.

use std::path::Path;

use anyhow::{anyhow, bail, Context};
use btf_rs::{Btf, Struct, Type};
use observer_common::Offsets;

pub const VMLINUX_BTF: &str = "/sys/kernel/btf/vmlinux";

pub fn resolve(path: &Path) -> anyhow::Result<Offsets> {
    let btf =
        Btf::from_file(path).with_context(|| format!("parsing BTF from {}", path.display()))?;
    from_btf(&btf)
}

fn from_btf(btf: &Btf) -> anyhow::Result<Offsets> {
    Ok(Offsets {
        task_pid: offset_of(btf, "task_struct", "pid")?,
        task_cgroups: offset_of(btf, "task_struct", "cgroups")?,
        css_set_dfl_cgrp: offset_of(btf, "css_set", "dfl_cgrp")?,
        cgroup_kn: offset_of(btf, "cgroup", "kn")?,
        kernfs_node_id: offset_of(btf, "kernfs_node", "id")?,
    })
}

/// The byte offset of `member` in the struct `name`. Fails on a missing member and on a member
/// that is not byte aligned, because the eBPF side only does byte addressed loads.
fn offset_of(btf: &Btf, name: &str, member: &str) -> anyhow::Result<u32> {
    let types = btf
        .resolve_types_by_name(name)
        .with_context(|| format!("kernel BTF has no struct {name}"))?;
    let definition = types
        .into_iter()
        .find_map(|t| match t {
            Type::Struct(s) if !s.members.is_empty() => Some(s),
            _ => None,
        })
        .ok_or_else(|| anyhow!("kernel BTF has no definition of struct {name}"))?;
    let (bits, bitfield) = member_bits(btf, &definition, member)?
        .ok_or_else(|| anyhow!("kernel BTF has no member {name}.{member}"))?;
    if bitfield || bits % 8 != 0 {
        bail!("{name}.{member} is not byte aligned (bit offset {bits})");
    }
    Ok(bits / 8)
}

/// The bit offset of `member` in `s`, and whether it is a bit field. Searches anonymous struct
/// and union members too, because the kernel wraps fields in them.
fn member_bits(btf: &Btf, s: &Struct, member: &str) -> anyhow::Result<Option<(u32, bool)>> {
    for m in &s.members {
        let name = btf.resolve_name(m).unwrap_or_default();
        if name == member {
            return Ok(Some((
                m.bit_offset(),
                m.bitfield_size().is_some_and(|b| b != 0),
            )));
        }
        if !name.is_empty() {
            continue;
        }
        let Some(inner) = composite(btf, btf.resolve_chained_type(m)?)? else {
            continue;
        };
        if let Some((bits, bitfield)) = member_bits(btf, &inner, member)? {
            return Ok(Some((m.bit_offset() + bits, bitfield)));
        }
    }
    Ok(None)
}

/// Strips typedefs and qualifiers until a struct or union remains.
fn composite(btf: &Btf, mut t: Type) -> anyhow::Result<Option<Struct>> {
    loop {
        t = match t {
            Type::Struct(s) | Type::Union(s) => return Ok(Some(s)),
            Type::Typedef(_)
            | Type::Const(_)
            | Type::Volatile(_)
            | Type::Restrict(_)
            | Type::TypeTag(_) => match t.as_btf_type() {
                Some(inner) => btf.resolve_chained_type(inner)?,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND_INT: u32 = 1;
    const KIND_PTR: u32 = 2;
    const KIND_STRUCT: u32 = 4;
    const KIND_UNION: u32 = 5;
    const KIND_FWD: u32 = 7;

    /// Writes a minimal BTF blob, so the tests do not depend on the kernel of the host.
    #[derive(Default)]
    struct BtfBuilder {
        types: Vec<u8>,
        strings: Vec<u8>,
        count: u32,
    }

    /// A member: name, type id, bit offset, bit field size.
    type Member<'a> = (&'a str, u32, u32, u32);

    impl BtfBuilder {
        fn new() -> Self {
            Self {
                strings: vec![0],
                ..Default::default()
            }
        }

        fn string(&mut self, s: &str) -> u32 {
            if s.is_empty() {
                return 0;
            }
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(s.as_bytes());
            self.strings.push(0);
            offset
        }

        fn header(&mut self, name: &str, kind: u32, vlen: u32, kind_flag: bool, size: u32) -> u32 {
            let name = self.string(name);
            let info = vlen | (kind << 24) | (u32::from(kind_flag) << 31);
            for word in [name, info, size] {
                self.types.extend_from_slice(&word.to_le_bytes());
            }
            self.count += 1;
            self.count
        }

        fn int(&mut self) -> u32 {
            let id = self.header("int", KIND_INT, 0, false, 4);
            self.types.extend_from_slice(&32u32.to_le_bytes());
            id
        }

        fn ptr(&mut self) -> u32 {
            self.header("", KIND_PTR, 0, false, 0)
        }

        fn fwd(&mut self, name: &str) -> u32 {
            self.header(name, KIND_FWD, 0, false, 0)
        }

        fn composite(&mut self, kind: u32, name: &str, size: u32, members: &[Member]) -> u32 {
            let kind_flag = members.iter().any(|m| m.3 != 0);
            let id = self.header(name, kind, members.len() as u32, kind_flag, size);
            for &(name, type_id, bits, bitfield) in members {
                let name = self.string(name);
                let offset = if kind_flag {
                    (bitfield << 24) | bits
                } else {
                    bits
                };
                for word in [name, type_id, offset] {
                    self.types.extend_from_slice(&word.to_le_bytes());
                }
            }
            id
        }

        fn build(self) -> Btf {
            let mut blob = Vec::new();
            blob.extend_from_slice(&0xeb9f_u16.to_le_bytes());
            blob.extend_from_slice(&[1, 0]);
            let types_len = self.types.len() as u32;
            // hdr_len, type_off, type_len, str_off, str_len
            for word in [24, 0, types_len, types_len, self.strings.len() as u32] {
                blob.extend_from_slice(&word.to_le_bytes());
            }
            blob.extend_from_slice(&self.types);
            blob.extend_from_slice(&self.strings);
            Btf::from_bytes(&blob).unwrap()
        }
    }

    /// The five structs, with `pid` inside an anonymous union inside an anonymous struct and a
    /// forward declaration of `cgroup` ahead of its definition.
    fn kernel_like() -> Btf {
        let mut b = BtfBuilder::new();
        let int = b.int();
        let ptr = b.ptr();
        let inner = b.composite(
            KIND_STRUCT,
            "",
            8,
            &[("tgid", int, 0, 0), ("pid", int, 32, 0)],
        );
        let union = b.composite(KIND_UNION, "", 8, &[("", inner, 0, 0), ("raw", int, 0, 0)]);
        b.composite(
            KIND_STRUCT,
            "task_struct",
            32,
            &[
                ("flags", int, 0, 0),
                ("", union, 64, 0),
                ("cgroups", ptr, 192, 0),
            ],
        );
        b.composite(
            KIND_STRUCT,
            "css_set",
            16,
            &[("refcount", int, 0, 0), ("dfl_cgrp", ptr, 64, 0)],
        );
        b.fwd("cgroup");
        b.composite(
            KIND_STRUCT,
            "cgroup",
            24,
            &[("self", int, 0, 0), ("kn", ptr, 128, 0)],
        );
        b.composite(
            KIND_STRUCT,
            "kernfs_node",
            64,
            &[("count", int, 0, 0), ("id", int, 448, 0)],
        );
        b.build()
    }

    #[test]
    fn resolves_all_offsets() {
        let offsets = from_btf(&kernel_like()).unwrap();
        assert_eq!(
            offsets,
            Offsets {
                task_pid: 12,
                task_cgroups: 24,
                css_set_dfl_cgrp: 8,
                cgroup_kn: 16,
                kernfs_node_id: 56,
            }
        );
    }

    #[test]
    fn missing_member_fails() {
        let mut b = BtfBuilder::new();
        let int = b.int();
        b.composite(KIND_STRUCT, "task_struct", 4, &[("tgid", int, 0, 0)]);
        let error = offset_of(&b.build(), "task_struct", "pid").unwrap_err();
        assert!(error.to_string().contains("task_struct.pid"), "{error}");
    }

    #[test]
    fn missing_struct_fails() {
        let mut b = BtfBuilder::new();
        b.fwd("cgroup");
        let error = offset_of(&b.build(), "cgroup", "kn").unwrap_err();
        assert!(error.to_string().contains("struct cgroup"), "{error}");
    }

    #[test]
    fn bit_field_fails() {
        let mut b = BtfBuilder::new();
        let int = b.int();
        b.composite(KIND_STRUCT, "task_struct", 4, &[("pid", int, 8, 16)]);
        let error = offset_of(&b.build(), "task_struct", "pid").unwrap_err();
        assert!(error.to_string().contains("not byte aligned"), "{error}");
    }

    #[test]
    fn unaligned_member_fails() {
        let mut b = BtfBuilder::new();
        let int = b.int();
        b.composite(KIND_STRUCT, "task_struct", 8, &[("pid", int, 4, 0)]);
        let error = offset_of(&b.build(), "task_struct", "pid").unwrap_err();
        assert!(error.to_string().contains("not byte aligned"), "{error}");
    }

    /// Runs where the host exposes kernel BTF, which CI and the image do.
    #[test]
    fn resolves_running_kernel() {
        let path = Path::new(VMLINUX_BTF);
        if !path.exists() {
            return;
        }
        let offsets = resolve(path).unwrap();
        assert_ne!(offsets.task_pid, 0);
        assert_ne!(offsets.task_cgroups, 0);
    }
}
