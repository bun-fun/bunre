//! `bun.cli.Command.Tag` — the top-level CLI subcommand discriminant.
//! bunre: stripped to runtime + bundler only (no package manager CLI, no discord, no fuzzilli).
//! `InstallCommand` is kept as a library-level token for `bun_install`'s internal
//! `load_config` call (it is never produced by the CLI dispatcher).

use enum_map::Enum;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Enum, core::marker::ConstParamTy)]
pub enum Tag {
    AutoCommand,
    HelpCommand,
    ReservedCommand,
    /// arg0 == 'node'
    RunAsNodeCommand,
    RunCommand,
    /// Library-only: `bun_install` passes this when loading bunfig. Never dispatched.
    InstallCommand,
}

impl Tag {
    pub const fn char(self) -> u8 {
        match self {
            Tag::AutoCommand => b'a',
            Tag::HelpCommand => b'h',
            Tag::ReservedCommand => b'w',
            Tag::RunAsNodeCommand => b'n',
            Tag::RunCommand => b'r',
            Tag::InstallCommand => b'i',
        }
    }

    pub fn read_global_config(self) -> bool {
        false
    }

    pub fn is_npm_related(self) -> bool {
        false
    }

    pub(crate) const COUNT: usize = <Self as Enum>::LENGTH;

    const ALL: [Self; Self::COUNT] = [
        Self::AutoCommand,
        Self::HelpCommand,
        Self::ReservedCommand,
        Self::RunAsNodeCommand,
        Self::RunCommand,
        Self::InstallCommand,
    ];
}

const _: () = {
    let mut seen = [false; 256];
    let mut i = 0;
    while i < Tag::COUNT {
        let c = Tag::ALL[i].char() as usize;
        assert!(
            !seen[c],
            "Tag::char() collision: two subcommands share a byte"
        );
        seen[c] = true;
        i += 1;
    }
};

#[repr(transparent)]
pub struct TagTable<V: 'static>(pub(crate) [V; Tag::COUNT]);

impl<V> core::ops::Index<Tag> for TagTable<V> {
    type Output = V;
    #[inline]
    fn index(&self, tag: Tag) -> &V {
        &self.0[tag as usize]
    }
}

pub static LOADS_CONFIG: TagTable<bool> = TagTable({
    let mut a = [false; Tag::COUNT];
    a[Tag::AutoCommand as usize] = true;
    a[Tag::RunCommand as usize] = true;
    a[Tag::RunAsNodeCommand as usize] = true;
    a
});

pub static ALWAYS_LOADS_CONFIG: TagTable<bool> = TagTable({
    let a = [false; Tag::COUNT];
    a
});

pub static USES_GLOBAL_OPTIONS: TagTable<bool> = TagTable({
    let a = [true; Tag::COUNT];
    a
});
