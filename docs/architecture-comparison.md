---
title: "BitTorrent Library Architecture Comparison"
subtitle: "torrent (Rust) vs rqbit (Rust) vs libtorrent-rasterbar (C++)"
date: "March 2026"
geometry: "margin=2cm"
fontsize: 10pt
mainfont: "Open Sans"
monofont: "Hack"
monofontoptions: "Scale=0.85"
colorlinks: true
linkcolor: "blue"
urlcolor: "blue"
header-includes:
  - \usepackage{booktabs}
  - \usepackage{longtable}
  - \usepackage{colortbl}
  - \usepackage{xcolor}
  - \usepackage{float}
  - \usepackage{fancyhdr}
  - \usepackage{enumitem}
  - \setlist{nosep}
  - \definecolor{headerblue}{HTML}{1a365d}
  - \definecolor{lightgray}{HTML}{f7f7f7}
  - \definecolor{checkgreen}{HTML}{22863a}
  - \definecolor{crossred}{HTML}{cb2431}
  - \definecolor{warnamber}{HTML}{b08800}
  - \pagestyle{fancy}
  - \fancyhead[L]{\small\textcolor{headerblue}{BitTorrent Architecture Comparison}}
  - \fancyhead[R]{\small\textcolor{headerblue}{March 2026}}
  - \fancyfoot[C]{\thepage}
  - \renewcommand{\headrulewidth}{0.4pt}
  - \renewcommand{\arraystretch}{1.3}
---

\newcommand{\yes}{\textcolor{checkgreen}{\textbf{Yes}}}
\newcommand{\no}{\textcolor{crossred}{\textbf{No}}}
\newcommand{\partialyes}{\textcolor{warnamber}{\textbf{Partial}}}

# 1. Overview

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Dimension}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Language & Rust & Rust & C++14 \\
\rowcolor{lightgray} Started & 2025 & 2022 & 2003 \\
License & GPL-3.0 & Custom/Other & BSD \\
\rowcolor{lightgray} Codebase Size & 31.5k LOC, 12 crates & 14 crates & 102k LOC, monolith \\
Tests & 1,386 & Undocumented & 108 unit + 32 sim \\
\rowcolor{lightgray} BEPs Implemented & 27 & \textasciitilde 16 & 30+ \\
v2 Torrents (BEP 52) & \yes & \no & \yes \\
\rowcolor{lightgray} Encryption (MSE/PE) & \yes & \no & \yes \\
uTP (BEP 29) & \yes{} (LEDBAT) & \partialyes{} (CUBIC) & \yes{} (LEDBAT) \\
\rowcolor{lightgray} Peak Throughput & 116 MB/s & 20 Gbps (claimed) & \textasciitilde 150 MB/s \\
\bottomrule
\end{tabular}
\end{table}

# 2. Architecture & Modularity

## Crate/Module Organization

**torrent** uses a 12-crate Rust workspace with strict layered dependencies. Each crate is independently usable — you can pull in `torrent-dht` without the session layer, or `torrent-bencode` as a standalone codec.

**rqbit** uses 14 crates, but the main `librqbit` is a monolith containing session, storage, HTTP API, peer logic, and streaming. Protocol crates are properly separated.

**libtorrent** is a single C++ library with logical module separation via directories, but everything links into one shared object. The two largest classes --- `session_impl` (7.4k lines) and `torrent` (12.4k lines) --- are monolithic.

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Extension Point}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Disk I/O Backend & Trait (3 impls) & Trait + middleware & Class (3 impls) \\
\rowcolor{lightgray} Choking Algorithm & Trait (pluggable) & None & Setting (2 modes) \\
Extension Protocol & Plugin trait & Hardcoded & Plugin class \\
\rowcolor{lightgray} Transport Layer & NetworkFactory trait & Hardcoded & Socket abstraction \\
Piece Picking & Configurable context & File-priority only & 7 built-in modes \\
\rowcolor{lightgray} Storage Middleware & No & Composable wrappers & Built-in (part files) \\
\bottomrule
\end{tabular}
\end{table}

# 3. Concurrency Model

This is the most consequential architectural divergence between the three implementations.

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Aspect}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Model & Actor (msg passing) & Shared state + tasks & Single thread + pools \\
\rowcolor{lightgray} Hot-Path Locks & None (actor-isolated) & RwLock, DashMap & None (single-threaded) \\
CPU Utilization & Multi-core & Multi-core & Single-core network \\
\rowcolor{lightgray} Scalability Ceiling & Channel backpressure & Lock contention & Single-thread CPU \\
Deadlock Risk & None & Low (documented) & None \\
\rowcolor{lightgray} Complexity & Medium & Low & Low \\
\bottomrule
\end{tabular}
\end{table}

**torrent** distributes work across all CPU cores via independent actor event loops. A slow torrent doesn't block others.

**rqbit** uses shared state behind `RwLock` with `Notify` signals. Simpler, but contention grows with peer count.

**libtorrent** runs all protocol logic on a single thread --- zero lock contention, but capped to one core.

# 4. Piece Selection

The piece picker directly determines download speed, swarm health, and completion time.

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Feature}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Rarest-First & \yes & \no & \yes \\
\rowcolor{lightgray} Sequential Mode & \yes & \no & \yes \\
Streaming Priority & \yes & \yes & \yes \\
\rowcolor{lightgray} End-Game Mode & \yes{} (4 req/peer) & \no & \yes{} (1 block/peer) \\
Extent Affinity & \yes & \no & \yes{} (4 MiB) \\
\rowcolor{lightgray} Piece Stealing & \yes{} (10x threshold) & \yes{} (10x + 3x) & \no \\
Suggested Pieces (BEP 6) & \yes & \no & \yes \\
\rowcolor{lightgray} Speed-Class Affinity & \yes & \no & \yes \\
Partial Piece Pref. & \yes & N/A (1 peer/piece) & \yes \\
\rowcolor{lightgray} Auto-Sequential & \yes & \no & \no \\
\bottomrule
\end{tabular}
\end{table}

Rarest-first is critical for **swarm health** --- without it, common pieces propagate while rare ones die when their sole seeder leaves. rqbit's lack of rarest-first makes it a less effective swarm participant. torrent's auto-sequential mode and piece stealing are unique features not found in either competitor.

# 5. Disk I/O Architecture

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Aspect}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
I/O Model & Async actor & Sync positioned I/O & Async worker pool \\
\rowcolor{lightgray} Write Strategy & Buffer + flush on complete & Direct pwrite/pwritev & Store buffer + mmap (2.0) \\
Read Cache & ARC & LRU write-through & OS page cache (2.0) \\
\rowcolor{lightgray} Hashing & Parallel (tokio::spawn) & Single (OpenSSL) & Dedicated thread pool \\
Backends & Posix, Mmap, Disabled & Filesystem, Mmap & Mmap, Posix, Disabled \\
\rowcolor{lightgray} Part Files & \no & \no & \yes \\
\bottomrule
\end{tabular}
\end{table}

**torrent** accumulates blocks in a `BTreeMap` per piece, flushing as a single sorted write on completion. The ARC cache provides application-level read intelligence.

**rqbit** writes each block directly via `pwrite`/`pwritev` --- deliberate simplicity, relying on OS and SSD write coalescing.

**libtorrent 2.0** delegated caching to the OS kernel via mmap. This lost application-level intelligence (can't suggest cached pieces, can't order flushes relative to hash cursor) and caused measurable regressions on Windows and HDDs. The 1.x ARC cache was superior but couldn't be ported forward.

# 6. Request Pipelining

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Aspect}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Default Depth & 128 fixed & 128 (Semaphore) & 5 initial, dynamic \\
\rowcolor{lightgray} Sizing Strategy & Fixed at max & Fixed burst on unchoke & rate * time / block \\
Max Depth & 128 & 128 & 500 \\
\rowcolor{lightgray} Adaptation & None & None & Dynamic per throughput \\
End-Game & 4 req/peer + cancel & None & 1 redundant block \\
\bottomrule
\end{tabular}
\end{table}

libtorrent's dynamic sizing is the most sophisticated: starts at 5 requests, doubles every RTT (application-level slow start), and targets 3 seconds of outstanding data. torrent and rqbit use aggressive fixed depth (128 $\times$ 16 KiB = 2 MiB in flight) --- fast on good connections but potentially wasteful on slow ones.

# 7. Protocol Coverage

\begin{table}[H]
\centering
\small
\begin{tabular}{p{4.5cm} p{2.8cm} p{2.8cm} p{2.8cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Protocol / BEP}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Core Protocol (BEP 3) & \yes & \yes & \yes \\
\rowcolor{lightgray} Fast Extension (BEP 6) & \yes & \no & \yes \\
DHT (BEP 5) & \yes & \yes & \yes \\
\rowcolor{lightgray} DHT Security (BEP 42) & \yes & \no & \yes \\
DHT Storage (BEP 44) & \yes & \no & \yes \\
\rowcolor{lightgray} DHT Infohash Index (BEP 51) & \yes & \no & \yes \\
Metadata Exchange (BEP 9) & \yes & \yes & \yes \\
\rowcolor{lightgray} Extension Protocol (BEP 10) & \yes & \yes & \yes \\
Peer Exchange (BEP 11) & \yes & \yes & \yes \\
\rowcolor{lightgray} Multitracker (BEP 12) & \yes & \yes & \yes \\
Local Discovery (BEP 14) & \yes & \yes & \yes \\
\rowcolor{lightgray} UDP Tracker (BEP 15) & \yes & \yes & \yes \\
Super Seeding (BEP 16) & \yes & \no & \yes \\
\rowcolor{lightgray} HTTP Seeding (BEP 17/19) & \yes & \no & \yes \\
Upload Only (BEP 21) & \yes & \no & \yes \\
\rowcolor{lightgray} Private Torrents (BEP 27) & \yes & \yes & \yes \\
uTP (BEP 29) & \yes{} (LEDBAT) & \partialyes{} (CUBIC) & \yes{} (LEDBAT) \\
\rowcolor{lightgray} SSL Torrents (BEP 35) & \yes & \no & \yes \\
Canonical Peer Priority (BEP 40) & \yes & \no & \yes \\
\rowcolor{lightgray} Pad Files (BEP 47) & \yes & \yes & \yes \\
Scrape (BEP 48) & \yes & \no & \yes \\
\rowcolor{lightgray} BitTorrent v2 (BEP 52) & \yes & \no & \yes \\
Magnet so= (BEP 53) & \yes & \yes & \yes \\
\rowcolor{lightgray} Holepunch (BEP 55) & \yes & \no & \yes \\
MSE/PE Encryption & \yes & \no & \yes \\
\rowcolor{lightgray} I2P Anonymous Network & \yes{} (SAM v3.1) & \no & \partialyes{} (no DHT) \\
IP Filtering & \yes & \no & \yes \\
\rowcolor{lightgray} Smart Banning & \yes & \no & \yes \\
Torrent Creation & \yes & \no & \yes \\
\rowcolor{lightgray} Fast Resume & \yes & \partialyes{} (probabilistic) & \yes \\
\bottomrule
\end{tabular}
\end{table}

# 8. Choking & Incentive Mechanism

The choking algorithm implements **tit-for-tat reciprocity** --- the game theory that makes BitTorrent work.

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Feature}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Tit-for-Tat & \yes & \no{} (always unchokes) & \yes \\
\rowcolor{lightgray} Optimistic Unchoke & \yes & \no & \yes{} (30s rotation) \\
Pluggable Strategies & Trait-based & None & Setting-based \\
\rowcolor{lightgray} Seed Strategies & 3 modes & None & 3 modes \\
Anti-Snub & \yes & \no & \yes{} (60s timeout) \\
\rowcolor{lightgray} Peer Turnover & \yes & \no & \yes{} (4\%/300s) \\
\bottomrule
\end{tabular}
\end{table}

rqbit always sends Unchoke to every peer, making it a **freeloader** in game-theory terms. If every client behaved this way, there would be no incentive to upload and the protocol would collapse.

# 9. uTP & Encryption

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Aspect}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
uTP Congestion Control & LEDBAT (correct) & CUBIC (wrong) & LEDBAT (correct) \\
\rowcolor{lightgray} Yields to TCP? & \yes & \no & \yes \\
Clock Drift Compensation & Standard & Unknown & Bidirectional baseline \\
\rowcolor{lightgray} Path MTU Discovery & Unknown & Unknown & Binary search + DF bit \\
uTP Status & Production & Experimental & Production (20+ yrs) \\
\rowcolor{lightgray} Encryption Protocol & MSE/PE (DH + RC4) & None & MSE/PE (DH + RC4) \\
Encryption Modes & Disabled/Enabled/Forced & N/A & Disabled/Enabled/Forced \\
\bottomrule
\end{tabular}
\end{table}

The entire point of uTP is LEDBAT congestion control --- low-priority traffic that yields to regular TCP. rqbit's use of CUBIC makes its uTP functionally ``TCP over UDP'' without the network-friendliness benefit.

No encryption means ISPs can trivially identify and throttle traffic, and peers requiring encryption will refuse connections.

# 10. Performance Design

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Metric}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Measured Throughput & 116 MB/s & ``20 Gbps'' (claim) & \textasciitilde 150 MB/s (tuned) \\
\rowcolor{lightgray} Memory Footprint & Moderate & Tens of MB & \textasciitilde 128 KB/torrent \\
Ramp-Up Time & Immediate & Immediate & <1s (slow start) \\
\rowcolor{lightgray} Write Path & Buffer $\to$ sorted flush & Direct pwrite & Store buffer $\to$ mmap \\
Alert/Event System & broadcast channel & Notify signals & Zero-alloc double-buffer \\
\rowcolor{lightgray} Stats Collection & Atomic counters & Atomic counters & Custom profilers \\
Scalability Target & Single client & Single client & 1M torrents (ghost) \\
\bottomrule
\end{tabular}
\end{table}

libtorrent's alert system (heterogeneous queue, double-buffered, stack-allocated strings) is the most optimized event delivery mechanism of the three. Its bdecode parser (8-byte packed tokens) is 77x faster than the original implementation.

# 11. Testing & Simulation

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Aspect}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
Unit Tests & 1,386 & Undocumented & 108 files \\
\rowcolor{lightgray} Simulation Framework & torrent-sim crate & \no & libsimulator \\
Deterministic Testing & \yes{} (virtual clock) & \no & \yes \\
\rowcolor{lightgray} Fuzzing & Unknown & Unknown & \yes \\
Disabled Disk Backend & \yes & Unknown & \yes \\
\rowcolor{lightgray} Network Simulation & NetworkFactory trait & \no & In-process network \\
\bottomrule
\end{tabular}
\end{table}

Both torrent and libtorrent invest heavily in simulation-based testing --- essential for verifying complex protocol interactions that can't be reliably tested with real networks.

# 12. NAT Traversal

\begin{table}[H]
\centering
\begin{tabular}{p{3.8cm} p{3.5cm} p{3.5cm} p{3.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Method}} & \textcolor{white}{\textbf{torrent}} & \textcolor{white}{\textbf{rqbit}} & \textcolor{white}{\textbf{libtorrent}} \\
\midrule
UPnP IGD & \yes & \yes & \yes \\
\rowcolor{lightgray} NAT-PMP & \yes & \no & \yes \\
PCP (RFC 6887) & \yes & \no & \no \\
\rowcolor{lightgray} Holepunch (BEP 55) & \yes & \no & \yes \\
Fallback Chain & PCP $\to$ NAT-PMP $\to$ UPnP & UPnP only & NAT-PMP $\to$ UPnP \\
\bottomrule
\end{tabular}
\end{table}

torrent supports PCP (the newest protocol, RFC 6887), which neither rqbit nor libtorrent implement.

\newpage

# 13. Verdict

## Ranking by Architecture Quality

\begin{table}[H]
\centering
\begin{tabular}{c p{2.2cm} p{10.5cm}}
\toprule
\rowcolor{headerblue}\textcolor{white}{\textbf{Rank}} & \textcolor{white}{\textbf{Library}} & \textcolor{white}{\textbf{Rationale}} \\
\midrule
\textbf{1} & \textbf{torrent} & Best modularity (12 independent crates), best concurrency model for modern multi-core hardware (actor model without locks), near-complete protocol coverage (27 BEPs), Rust memory safety. Needs maturity and hot-path optimization. \\
\rowcolor{lightgray} \textbf{2} & \textbf{libtorrent} & Most optimized data structures, broadest protocol support (30+ BEPs), 20 years of production hardening. Held back by monolithic design, mmap regression, single-threaded ceiling, and C++ safety gaps. \\
\textbf{3} & \textbf{rqbit} & Simplest and lightest, good for basic use. Missing too many fundamental protocols and algorithms (encryption, rarest-first, tit-for-tat, v2, end-game) for a high-performance engine. \\
\bottomrule
\end{tabular}
\end{table}

## Key Gaps for torrent to Close

\begin{enumerate}
\item \textbf{Dynamic request queue sizing} --- adopt libtorrent's \texttt{request\_queue\_time * rate / block\_size} formula
\item \textbf{O(1) piece picker operations} --- swap-based bucket boundaries for large torrents
\item \textbf{Application-level slow start} --- match TCP slow start for cleaner ramp-up
\item \textbf{Cache-line optimization} --- profile hot paths, pack frequently accessed fields
\item \textbf{Production hardening} --- diverse real-world swarms, profiling, iteration
\end{enumerate}

## The Bottom Line

torrent's architecture is \textbf{designed to surpass libtorrent}. The actor model distributes protocol processing across all cores while maintaining lock-free properties. Rust's ownership system eliminates entire categories of bugs at compile time. The 12-crate workspace enables clean reuse. 27 BEPs demonstrate that feature completeness doesn't require monolithic design.

libtorrent has the strongest \textit{execution} after 20 years of refinement. torrent has the strongest \textit{foundation} for the next 20 years. The question is whether torrent can close the optimization gap before libtorrent's architectural limitations become insurmountable.
