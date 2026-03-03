#!/usr/bin/env python3
"""Download a torrent using libtorrent and exit on completion."""
import sys
import time
import libtorrent as lt

def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <magnet_or_torrent> <output_dir>", file=sys.stderr)
        sys.exit(1)

    source = sys.argv[1]
    output_dir = sys.argv[2]

    ses = lt.session()
    ses.listen_on(6891, 6899)

    settings = ses.get_settings()
    settings["alert_mask"] = lt.alert.category_t.all_categories
    ses.apply_settings(settings)

    if source.startswith("magnet:"):
        handle = lt.add_magnet_uri(ses, source, {"save_path": output_dir})
    else:
        info = lt.torrent_info(source)
        handle = ses.add_torrent({"ti": info, "save_path": output_dir})

    print(f"Downloading to {output_dir}...", file=sys.stderr)
    start = time.monotonic()

    while not handle.is_seed():
        s = handle.status()
        print(
            f"\r{s.progress * 100:.1f}% | "
            f"down {s.download_rate / 1024:.0f} KB/s | "
            f"{s.num_peers} peers",
            end="",
            file=sys.stderr,
        )
        time.sleep(1)

    elapsed = time.monotonic() - start
    s = handle.status()
    total_mb = s.total_done / (1024 * 1024)
    avg_speed = total_mb / elapsed if elapsed > 0 else 0
    print(f"\nDone in {elapsed:.1f}s ({avg_speed:.1f} MB/s)", file=sys.stderr)

if __name__ == "__main__":
    main()
