//! Audited primary-tree scope, not a reconstruction of every build input.
//! Supplemental checkouts are never fetched at HEAD. Gitlinks in the selected
//! tree determine which submodules are acquired and their exact revisions.
use url::Url;

pub(super) struct Rule {
    pub primary: &'static str,
    pub supplemental: &'static [&'static str],
}

macro_rules! rule {
    ($primary:literal; $($secondary:literal),* $(,)?) => {
        Some(&Rule { primary: $primary, supplemental: &[$($secondary),*] })
    };
}

pub(super) fn rule(base: &str) -> Option<&'static Rule> {
    match base {
        "coreutils" => {
            rule!("https://git.savannah.gnu.org/git/coreutils"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "cpio" => {
            rule!("https://git.savannah.gnu.org/git/cpio"; "https://git.savannah.gnu.org/git/gnulib", "https://git.savannah.gnu.org/git/paxutils")
        }
        "diffutils" => {
            rule!("https://git.savannah.gnu.org/git/diffutils"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "findutils" => {
            rule!("https://git.savannah.gnu.org/git/findutils"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "grep" => {
            rule!("https://git.savannah.gnu.org/git/grep"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "grub" => {
            rule!("https://git.savannah.gnu.org/git/grub"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "gzip" => {
            rule!("https://git.savannah.gnu.org/git/gzip"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "inetutils" => {
            rule!("https://git.savannah.gnu.org/git/inetutils"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "libtool" => {
            rule!("https://git.savannah.gnu.org/git/libtool"; "https://git.savannah.gnu.org/git/gnulib", "https://github.com/gnulib-modules/bootstrap")
        }
        "m4" => {
            rule!("https://git.savannah.gnu.org/git/m4"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "patch" => {
            rule!("https://git.savannah.gnu.org/git/patch"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "tar" => {
            rule!("https://git.savannah.gnu.org/git/tar"; "https://git.savannah.gnu.org/git/gnulib", "https://git.savannah.gnu.org/git/paxutils")
        }
        "wget" => {
            rule!("https://gitlab.com/gnuwget/wget"; "https://git.savannah.gnu.org/git/gnulib")
        }
        "enchant" => {
            rule!("https://github.com/rrthomas/enchant"; "https://git.savannah.gnu.org/git/gnulib", "https://github.com/gnulib-modules/bootstrap")
        }
        "glib2" | "lib32-glib2" => {
            rule!("https://gitlab.gnome.org/GNOME/glib"; "https://gitlab.gnome.org/GNOME/gvdb")
        }
        "glycin" => {
            rule!("https://gitlab.gnome.org/GNOME/glycin"; "https://gitlab.gnome.org/sophie-h/test-images")
        }
        "graphene" => {
            rule!("https://github.com/ebassi/graphene"; "https://github.com/ebassi/mutest")
        }
        "libnghttp2" | "lib32-libnghttp2" => {
            rule!("https://github.com/nghttp2/nghttp2"; "https://github.com/ngtcp2/munit", "https://github.com/mruby/mruby", "https://github.com/tatsuhiro-t/neverbleed")
        }
        "libnghttp3" | "lib32-libnghttp3" => {
            rule!("https://github.com/ngtcp2/nghttp3"; "https://github.com/ngtcp2/munit", "https://github.com/ngtcp2/sfparse")
        }
        "libngtcp2" | "lib32-libngtcp2" => {
            rule!("https://github.com/ngtcp2/ngtcp2"; "https://github.com/ngtcp2/munit", "https://github.com/ngtcp2/urlparse")
        }
        "p11-kit" | "lib32-p11-kit" => {
            rule!("https://github.com/p11-glue/p11-kit"; "https://github.com/p11-glue/pkcs11-json")
        }
        "pcre2" | "lib32-pcre2" => {
            rule!("https://github.com/PCRE2Project/pcre2"; "https://github.com/zherczeg/sljit")
        }
        "libbluray" => {
            rule!("https://code.videolan.org/videolan/libbluray"; "https://code.videolan.org/videolan/libudfread")
        }
        "libjxl" => {
            rule!("https://github.com/libjxl/libjxl"; "https://github.com/mm2/Little-CMS", "https://github.com/webmproject/sjpeg", "https://github.com/google/skcms", "https://github.com/libjxl/testdata", "https://github.com/libjpeg-turbo/libjpeg-turbo")
        }
        "libldac" => {
            rule!("https://github.com/EHfive/ldacBT"; "https://android.googlesource.com/platform/external/libldac")
        }
        "malcontent" => {
            rule!("https://gitlab.freedesktop.org/pwithnall/malcontent"; "https://gitlab.gnome.org/GNOME/gvdb")
        }
        "libshout" => {
            rule!("https://gitlab.xiph.org/xiph/icecast-libshout"; "https://gitlab.xiph.org/xiph/icecast-common", "https://gitlab.xiph.org/xiph/icecast-m4")
        }
        "libtorrent-rasterbar" => {
            rule!("https://github.com/arvidn/libtorrent"; "https://github.com/arvidn/libsimulator", "https://github.com/arvidn/try_signal", "https://github.com/paullouisageneau/boost-asio-gnutls", "https://github.com/paullouisageneau/libdatachannel", "https://github.com/SergiusTheBest/plog", "https://github.com/paullouisageneau/usrsctp", "https://github.com/paullouisageneau/libjuice", "https://github.com/nlohmann/json", "https://github.com/cisco/libsrtp")
        }
        "lutris" => {
            rule!("https://github.com/lutris/lutris"; "https://github.com/bartok765/galaxy_blizzard_plugin")
        }
        "mbedtls" | "mbedtls3" => {
            rule!("https://github.com/Mbed-TLS/mbedtls"; "https://github.com/Mbed-TLS/mbedtls-framework", "https://github.com/Mbed-TLS/TF-PSA-Crypto")
        }
        "obs-studio" => {
            rule!("https://github.com/obsproject/obs-studio"; "https://github.com/obsproject/libdshowcapture", "https://github.com/obsproject/obs-browser", "https://github.com/obsproject/obs-websocket")
        }
        "ostree" => {
            rule!("https://github.com/ostreedev/ostree"; "https://github.com/mendsley/bsdiff", "https://gitlab.gnome.org/GNOME/libglnx")
        }
        "pahole" => {
            rule!("https://git.kernel.org/pub/scm/devel/pahole/pahole"; "https://github.com/libbpf/libbpf")
        }
        "python-lark-parser" => {
            rule!("https://github.com/lark-parser/lark"; "https://github.com/Hardmath123/nearley")
        }
        "python-numpy" => {
            rule!("https://github.com/numpy/numpy"; "https://github.com/numpy/meson", "https://github.com/google/highway", "https://github.com/intel/x86-simd-sort", "https://github.com/mreineck/pocketfft", "https://github.com/numpy/SVML", "https://github.com/python/pythoncapi-compat")
        }
        "python-rapidfuzz" => {
            rule!("https://github.com/rapidfuzz/RapidFuzz"; "https://github.com/taskflow/taskflow")
        }
        "python-referencing" => {
            rule!("https://github.com/python-jsonschema/referencing"; "https://github.com/python-jsonschema/referencing-suite")
        }
        "qt6-tools" => {
            rule!("https://code.qt.io/qt/qttools"; "https://code.qt.io/playground/qlitehtml")
        }
        "qt6-webengine" => {
            rule!("https://code.qt.io/qt/qtwebengine"; "https://code.qt.io/qt/qtwebengine-chromium")
        }
        "qtcreator" => {
            rule!("https://code.qt.io/qt-creator/qt-creator"; "https://code.qt.io/qt-creator/perfparser", "https://code.qt.io/playground/qlitehtml")
        }
        "gamescope" => {
            rule!("https://github.com/ValveSoftware/gamescope"; "https://gitlab.freedesktop.org/emersion/libdisplay-info", "https://gitlab.freedesktop.org/emersion/libliftoff", "https://github.com/ValveSoftware/openvr", "https://github.com/Joshua-Ashton/reshade", "https://github.com/KhronosGroup/SPIRV-Headers", "https://github.com/Joshua-Ashton/vkroots", "https://gitlab.freedesktop.org/wlroots/wlroots")
        }
        _ => None,
    }
}

pub(super) fn repository_matches(location: &str, expected: &str) -> bool {
    let Ok(url) = Url::parse(location) else {
        return false;
    };
    let location = url.as_str().trim_end_matches('/');
    let location = location.strip_suffix(".git").unwrap_or(location);
    url.query().is_none()
        && url.fragment().is_none()
        && location == expected
}

// GNU recipes sometimes carry both a pinned checkout and its release archive.
// This recognizes the alternate distribution only alongside the expected Git
// primary. Archives for other components are not swallowed by a suffix rule.
pub(super) fn alternate_distribution(base: &str, location: &str, version: &str) -> bool {
    if !matches!(
        base,
        "coreutils"
            | "cpio"
            | "diffutils"
            | "findutils"
            | "grep"
            | "grub"
            | "gzip"
            | "inetutils"
            | "libtool"
            | "m4"
            | "patch"
            | "tar"
            | "wget"
    ) {
        return false;
    }
    let Ok(url) = Url::parse(location) else {
        return false;
    };
    if url.origin().ascii_serialization() != "https://ftp.gnu.org"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(file) = url.path().strip_prefix(&format!("/gnu/{base}/{base}-")) else {
        return false;
    };
    [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.lz"]
        .iter()
        .any(|suffix| file == format!("{version}{suffix}"))
}
