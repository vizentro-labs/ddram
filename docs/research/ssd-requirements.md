# SSD requirements research for ddramnvme

Status: research notes, Oct 2026. Scope: what an NVMe/SSD tier used as a RAM extension has to respect (health telemetry, endurance, latency, thermal behaviour), how to read drive health on Linux, Windows and macOS without `smartctl`, and the engineering requirements that follow.

> **Sources and limits.** Everything here comes from public sources. **We could not read any private or customer requirement documents** (hyperscaler or OEM procurement specs, NDA vendor datasheets, JEDEC or NVMe documents behind a paywall or login). From this research environment the NVMe Base Specification PDF, the OCP specification PDFs, JEDEC and learn.microsoft.com could not be fetched directly. Byte layouts were therefore taken from source code that the sources below show to implement the specifications: the Linux kernel, libnvme and nvme-cli, smartmontools, QEMU, and Microsoft's own docs repository on GitHub. Each statement that could not be checked against a primary source carries the label **(unverified)**.

---

## 1. NVMe SMART / Health Information log (Log Identifier 02h)

- **Size and multi-byte fields:** the log is 512 bytes. All multi-byte fields are little-endian.
- **Retrieval:** Get Log Page (admin opcode 02h) with LID = 02h, normally with NSID = FFFFFFFFh, which returns the controller-wide log. A drive also reports a per-namespace log if Identify Controller LPA bit 0 is set.
- **How the layout was cross-checked:**
  - The Linux kernel `struct nvme_smart_log` in [`include/linux/nvme.h`](https://github.com/torvalds/linux/blob/master/include/linux/nvme.h).
  - libnvme `struct nvme_smart_log` in [`src/nvme/types.h`](https://github.com/linux-nvme/libnvme/blob/master/src/nvme/types.h). Its kernel-doc comments quote the NVMe Base Spec field definitions word for word.
  - Microsoft's `NVME_HEALTH_INFO_LOG` usage in the [Win32 docs](https://github.com/MicrosoftDocs/win32/blob/docs/desktop-src/FileIO/working-with-nvme-devices.md) (`Temperature[1]<<8 | Temperature[0]`, a 2-byte Kelvin value at offset 1).
  - The two C structs are field-for-field identical. Summing their member sizes gives the offsets below and a total of exactly 512 bytes.

### 1.1 Offset table

| Offset (dec) | Offset (hex) | Len | Field | Units / semantics |
|---:|---:|---:|---|---|
| 0 | 0x00 | 1 | **Critical Warning** | Bitfield. The bits show the current state and do not persist (see 1.2). |
| 1 | 0x01 | 2 | **Composite Temperature** | Kelvin, u16 LE (°C = K − 273). Unaligned. Compare against WCTEMP and CCTEMP from Identify Controller. |
| 3 | 0x03 | 1 | **Available Spare** | Normalized %, 0–100. |
| 4 | 0x04 | 1 | **Available Spare Threshold** | %, 0–100. Values 101–255 are reserved. |
| 5 | 0x05 | 1 | **Percentage Used** | Vendor estimate of life used, in %. Can exceed 100; values above 254 read as 255. Updated once per power-on hour. |
| 6 | 0x06 | 1 | **Endurance Group Critical Warning Summary** | Bitfield (NVMe 1.4+), see 1.2. |
| 7 | 0x07 | 25 | Reserved | |
| 32 | 0x20 | 16 | **Data Units Read** | u128. 1 unit = 1000 × 512 B = 512,000 B, rounded up. Excludes metadata. 0 means "not reported". |
| 48 | 0x30 | 16 | **Data Units Written** | u128, same units. Write Zeroes and Write Uncorrectable are not counted. |
| 64 | 0x40 | 16 | **Host Read Commands** | u128. Read plus Compare commands completed. |
| 80 | 0x50 | 16 | **Host Write Commands** | u128. Write commands completed. |
| 96 | 0x60 | 16 | **Controller Busy Time** | u128, **minutes** with an I/O command outstanding. |
| 112 | 0x70 | 16 | **Power Cycles** | u128. |
| 128 | 0x80 | 16 | **Power On Hours** | u128. May exclude non-operational power states. |
| 144 | 0x90 | 16 | **Unsafe Shutdowns** | u128. Counts power loss with no prior CC.SHN shutdown notification. |
| 160 | 0xA0 | 16 | **Media and Data Integrity Errors** | u128. Unrecovered ECC, CRC and LBA-tag errors. |
| 176 | 0xB0 | 16 | **Number of Error Information Log Entries** | u128, over the controller's life. |
| 192 | 0xC0 | 4 | **Warning Composite Temperature Time** | u32, **minutes** with WCTEMP ≤ temp < CCTEMP. Always 0 if WCTEMP or CCTEMP is 0. |
| 196 | 0xC4 | 4 | **Critical Composite Temperature Time** | u32, **minutes** with temp ≥ CCTEMP. |
| 200 | 0xC8 | 2 | Temperature Sensor 1 | Kelvin u16. 0 = not implemented. |
| 202 | 0xCA | 2 | Temperature Sensor 2 | " |
| 204 | 0xCC | 2 | Temperature Sensor 3 | " |
| 206 | 0xCE | 2 | Temperature Sensor 4 | " |
| 208 | 0xD0 | 2 | Temperature Sensor 5 | " |
| 210 | 0xD2 | 2 | Temperature Sensor 6 | " |
| 212 | 0xD4 | 2 | Temperature Sensor 7 | " |
| 214 | 0xD6 | 2 | Temperature Sensor 8 | " |
| 216 | 0xD8 | 4 | **Thermal Mgmt Temp 1 Transition Count** | u32. Saturates at FFFFFFFFh. Counts entries into host-controlled thermal management level 1 (light throttling). |
| 220 | 0xDC | 4 | **Thermal Mgmt Temp 2 Transition Count** | u32. Level 2 (heavy throttling). |
| 224 | 0xE0 | 4 | **Total Time for Thermal Mgmt Temp 1** | u32, **seconds**. |
| 228 | 0xE4 | 4 | **Total Time for Thermal Mgmt Temp 2** | u32, **seconds**. |
| 232 | 0xE8 | 280 | Reserved | Runs to byte 511. |

Parser notes:

- Read u128 counters as two u64 LE values (lo at +0, hi at +8). Real values fit in u64 today, but don't truncate silently: saturate or keep the full u128.
- Bytes = `data_units × 512_000`. A count of 0 can mean "not reported".
- The thresholds come from **Identify Controller** (CNS 01h, 4096 B). These offsets were computed from the Linux `struct nvme_id_ctrl`, which totals 4096 B:
  - `WCTEMP` at bytes 266–267 (Kelvin, 0 = not reported)
  - `CCTEMP` at 268–269
  - `MDTS` at 77
  - `LPA` at 261
  - `MNTMT` and `MXTMT` at 324 and 326
  - `VWC` at 525 (volatile write cache present)

### 1.2 Bitfields

**Critical Warning (byte 0).** Sources: libnvme `enum nvme_smart_crit` and kernel `NVME_SMART_CRIT_*`.

| Bit | Meaning |
|---|---|
| 0 | Available spare fell below the threshold. |
| 1 | Temperature is ≥ an over-temperature threshold or ≤ an under-temperature threshold. |
| 2 | NVM subsystem reliability is degraded (media or internal errors). |
| 3 | All media is read-only. This is not set for namespace write protection. |
| 4 | The volatile memory backup device has failed. This is the PLP capacitor, and the bit is valid only on drives that have one. |
| 5 | The Persistent Memory Region became read-only or unreliable (NVMe 1.4+; the kernel enum stops at bit 4). |
| 6–7 | Reserved. |

**Endurance Group Critical Warning Summary (byte 6).** Source: libnvme `enum nvme_smart_egcw`.

| Bit | Meaning |
|---|---|
| 0 | Spare below threshold in some endurance group. |
| 1 | Reserved. |
| 2 | Reliability degraded. |
| 3 | Read-only. |

For finer-grained endurance data, NVMe 1.4+ also defines the **Endurance Group Information log (LID 09h)**, which holds Percentage Used, Endurance Estimate, Data Units Written and Media Units Written per endurance group. It is worth reading when the controller supports it.

---

## 2. OCP Datacenter NVMe SSD Specification

### 2.1 What it is and how it reflects customer requirements

- **Who writes it:** the OCP Datacenter NVMe SSD spec is the public, multi-vendor statement of what hyperscalers require from datacenter SSDs. Contributors include Meta, Microsoft, Google, Dell and HPE.
- **Versions:**
  - v2.5 added human-readable telemetry, security changes and FDP ([FMS 2024 OCP update](https://files.futurememorystorage.com/proceedings/2024/20240806_DCTR-102-1_Stenfort.pdf)).
  - v2.6 (Sep 2024) and v2.6.1 followed. A [QEMU patch](https://git.cs.tu-dortmund.de/SYS-OSS/FRET-qemu/commit/23a4b3ebc74bbc6b6f44edf2255864042f122b82) cites 2.6.1 as current in Nov 2024 and puts the C0h log in §4.8.6.
  - v2.7 (Jan 2026) is listed on [opencompute.org](https://www.opencompute.org/documents/datacenter-nvme-ssd-specification-v2-7-final-pdf-1). Earlier documents: [v2.6](https://www.opencompute.org/documents/datacenter-nvme-ssd-specification-v2-6-2-pdf/), [v2.5](https://opencompute.org/documents/datacenter-nvme-ssd-specification-v2-5-pdf).
- **What it covers:**
  - NVMe and PCIe feature requirements, reliability, endurance, thermal, form factor, SMBus/NVMe-MI, security and labelling. These are the test groups in the [Teledyne LeCroy OCP 2.0 compliance suite](https://de.teledynelecroy.com/files/pdf/ocp2.0_compliance_test_suite_ds.pdf).
  - Long-running server reliability: temperature and power cycling ([Allion](https://www.allion.com/?p=25496)).
  - A minimum standard beyond the mandatory NVMe features ([SMART Modular](https://smartm.com/technology/129)).
- **How customers use it:** a typical hyperscaler or OEM requirement document reads "OCP Datacenter NVMe SSD spec vX.Y compliant" plus a customer-specific profile. That profile holds capacity, form factor (E1.S/E3.S/U.2), DWPD class, power envelope, QoS latency targets per percentile, firmware and security policy, and telemetry and fleet-health logs. Those private profiles were **not** accessible.

Typical public-visible requirement themes:

- **Health and telemetry:** standard SMART (02h) plus the OCP **SMART/Health Information Extended log C0h**, Error Recovery log (C1h), **Latency Monitor log (C3h)**, Device Capabilities (C4h), Unsupported Requirements (C5h), Hardware Component (C6h), TCG Configuration (C7h), Telemetry String log (C9h) and Latency Monitor Debug Telemetry (CAh). The IDs come from the nvme-cli OCP plugin [`ocp-nvme.h`](https://github.com/linux-nvme/nvme-cli/blob/master/plugins/ocp/ocp-nvme.h). NVMe Telemetry Host- and Controller-Initiated logs (07h/08h) are also required, with human-readable telemetry from v2.5.
- **QoS:** read-latency requirements are written as percentile tables (99 / 99.9 / 99.99 / 99.999 / 99.9999 %), typically for 4 KiB random read at low QD and at saturation, plus mixed 70/30 workloads. The numeric targets sit in the customer profile; the spec mainly standardises how latency is measured and reported, which is what the Latency Monitor log C3h is for **(unverified: the exact table layout in v2.6/2.7)**. Vendors advertise this kind of figure, for example Micron's "≤2 ms at 99.9999 % for 4 KiB mixed random" on the 7450 ([reseller copy of Micron spec](https://us.stockinthechannel.com/Product/Micron-7450-PRO-U-3-1-92-TB-PCI-Express-4-0-3D-TLC-NAND-NVMe/48370214)).
- **Endurance:** stated as DWPD over a warranty period (commonly 1 DWPD "read-intensive" or 3 DWPD "mixed-use" over 5 years), measured with the JEDEC JESD219 enterprise workload ([SNIA endurance white paper](https://www.snia.org/sites/default/files/SSSI/NVMe_SAS_SATA_Endurance_White_Paper.pdf)).
- **Power-loss protection (PLP):** after an unexpected power loss, all acknowledged writes must be durable. PLP health is reported through Critical Warning bit 4, C0h *Capacitor Health*, *PLP Start Count* and *Incomplete Shutdowns*.
- **Thermal:** the drive must report throttling (C0h bytes 96–97). OCP conformance scripts heat the drive and check that C0h reports the throttling state and count ([SANBlaze OCP 2.6 test group 7](https://sanblaze.ellisys.com/scripts/OCP_2_6_Group_7.html)).

### 2.2 C0h SMART / Health Information Extended log (512 B)

- **Layout source:** nvme-cli [`plugins/ocp/ocp-smart-extended-log.h`](https://github.com/linux-nvme/nvme-cli/blob/master/plugins/ocp/ocp-smart-extended-log.h), which carries byte ranges as comments. That covers OCP 2.0 through 2.5+, and newer fields are reserved/zero on older drives.
- **Validate the GUID before trusting the page:** bytes 496–511 hold GUID `AFD514C9-7C6F-4F9C-A4F2-BFEA2810AFC5`. On the wire it is the little-endian byte string `C5 AF 10 28 EA BF F2 A4 9C 4F 6F 7C C9 14 D5 AF`, per `scao_guid` in [`ocp-smart-extended-log.c`](https://github.com/linux-nvme/nvme-cli/blob/master/plugins/ocp/ocp-smart-extended-log.c).
- **Windows:** the SDK also ships `NVME_OCP_DEVICE_SMART_INFORMATION_LOG_V3` ([windows-rs docs](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Storage/Nvme/struct.NVME_OCP_DEVICE_SMART_INFORMATION_LOG_V3.html)).

| Bytes | Len | Field | Use for ddramnvme |
|---|---:|---|---|
| 0–15 | 16 | **Physical Media Units Written** (NAND writes; bytes per OCP **(unverified unit)**) | **Real WAF** = PMUW / (DataUnitsWritten × 512000). |
| 16–31 | 16 | Physical Media Units Read | |
| 32–37 / 38–39 | 6+2 | Bad User NAND Blocks raw / normalized | |
| 40–45 / 46–47 | 6+2 | Bad System NAND Blocks raw / normalized | |
| 48–55 | 8 | XOR Recovery Count | Rising means media is degrading. |
| 56–63 | 8 | Uncorrectable Read Error Count | |
| 64–71 | 8 | Soft ECC Error Count | |
| 72–75 | 4 | End-to-End Detected Errors | |
| 76–79 | 4 | End-to-End Corrected Errors | |
| 80 | 1 | System Data % Used | |
| 81–87 | 7 | Refresh Counts (read-disturb/retention refresh) | |
| 88–91 | 4 | User Data Erase Count max | P/E cycles, hottest block. |
| 92–95 | 4 | User Data Erase Count min | |
| 96 | 1 | Thermal Throttling Event Count | |
| 97 | 1 | Thermal Throttling Current Status | 00h = not throttled; non-zero = throttle level **(unverified encoding)**. |
| 98 / 99–100 / 101–102 / 103 | 1/2/2/1 | DSSD spec errata / point / minor / major version | |
| 104–111 | 8 | PCIe Correctable Error Count | |
| 112–115 | 4 | Incomplete Shutdowns | |
| 120 | 1 | % Free Blocks | |
| 128–129 | 2 | Capacitor Health (PLP) | %; FFFFh likely means "no capacitor" **(unverified)**. |
| 130–133 | 4 | NVMe base / cmd-set / PCIe / MI errata versions | |
| 136–143 | 8 | Unaligned I/O count | Should stay ~0 for our I/O. |
| 144–151 | 8 | Security Version Number | |
| 152–159 | 8 | Total NUSE | |
| 160–175 | 16 | PLP Start Count | |
| 176–191 | 16 | **Endurance Estimate** | Rated lifetime writes (unit per spec). |
| 192–199 | 8 | PCIe Link Retraining Count | |
| 200–207 | 8 | Power State Change Count | |
| 208–215 | 8 | Lowest Permitted FW Revision | |
| 216–221 | 2+2+2 | Total media dies / die-failure tolerance / dies offline | |
| 222 | 1 | Max Temperature Recorded | |
| 223 | 1 | Form factor | |
| 224–231 | 8 | NAND Avg Erase Count | |
| 232–235 | 4 | Command Timeouts | |
| 236–259 | | System area program-fail / uncorrectable-read / erase-fail counts (raw u32 + normalized u8, each padded to 8 B) | |
| 260–263 | 2+2 | Max peak power capability / current max avg power | |
| 264–269 | 6 | Lifetime power consumed | |
| 270–357 | 8+16+64 | DSSD FW revision / build UUID / build label | |
| 358–365 | 6+2 | Die-in-use bad NAND blocks raw / normalized | |
| 494–495 | 2 | Log Page Version | |
| 496–511 | 16 | Log Page GUID | Must match the GUID above. |

### 2.3 C3h Latency Monitor log (512 B)

- **Layout source:** nvme-cli `struct ssd_latency_monitor_log`.
- **Fields:**
  - feature status (0x00)
  - active bucket timer and threshold (0x02/0x04)
  - thresholds A–D (0x06–0x09)
  - **active bucket counters `u32[4][4]` (0x20–0x5F)**
  - active latency timestamps `u64[4][3]` (0x60)
  - active measured latency `u16[4][3]` (0xC0)
  - static bucket counters (0xF0–0x12F), timestamps (0x130) and measured latency (0x190)
  - debug-log trigger fields (0x1B4–0x1D0)
  - version (0x1EE) and GUID (0x1F0)
- **How it works:** the device histograms command latency into 4 configurable buckets per command class (read / write / deallocate). The host configures it through OCP Set Feature C5h ("Latency Monitor").
- **Use for ddramnvme:** a cheap, device-side tail-latency signal to log alongside our own userspace histograms. It is optional and present only on OCP drives.

---

## 3. Endurance

### 3.1 JEDEC JESD218 / JESD219

- **JESD218 (SSD requirements and endurance test method)** defines the application classes. Values below are from JESD218B.01 Table 1 as reproduced by [SNIA](https://www.snia.org/sites/default/files/SSSI/NVMe_SAS_SATA_Endurance_White_Paper.pdf) and [Transcend](https://se.transcend-info.com/Embedded/Essay-12).

| Class | Workload | Active use (power on) | Retention (power off) | Functional failure rate | UBER |
|---|---|---|---|---|---|
| Client | Client (JESD219A) | 8 h/day @ 40 °C | **1 year @ 30 °C** | ≤ 3 % | ≤ 10⁻¹⁵ |
| Enterprise | Enterprise (JESD219) | 24 h/day @ 55 °C | **3 months @ 40 °C** | ≤ 3 % | ≤ 10⁻¹⁶ |

  - TBW is the host writes a drive can absorb while still meeting these retention, FFR and UBER targets at end of life. Enterprise retention is shorter, which allows more P/E cycles from the same NAND.
  - JEDEC published a JESD218 update in May 2025 ([jedec.org](https://www.jedec.org/node/5866)). Its numbers could not be checked here.
- **JESD219 (endurance workloads):**
  - The *enterprise* workload is full-LBA-span random with an SPC-1 lineage ([Cox, FMS 2011](https://files.futurememorystorage.com/proceedings/2011/20110810_T1B_Cox.pdf)).
  - Transfer sizes range from 512 B to 64 KiB, with **4 KiB ≈ 67 %** of I/Os.
  - It is skewed: **50 % of accesses hit 5 % of LBAs, 30 % hit 15 %, and 20 % hit 80 %** (fio's [`examples/jesd219.fio`](https://git.stg.centos.org/source-git/fio/blob/0e769b22dad80e324206485579913ebfae8ca284/f/examples/jesd219.fio), a community reproduction).
  - It is 40/60 read/write and includes unaligned and sub-4K writes, so WAF is high (≈ 2–5 on typical OP).
  - JESD219A adds a trace-based *client* workload.
  - Micron argues the enterprise workload is dated ([Micron blog](https://www.micron.com/about/blog/storage/ssd/how-server-ssd-endurance-workload-specifications-are-like-tires)).
  - **Implication:** a sequential, 4K-aligned, large-block write stream from ddramnvme usually gets *more* than the rated TBW, because device WAF is close to 1. We should still budget against the rating.

### 3.2 Definitions and formulas

- **TBW** is total host bytes written over the warranty (decimal TB).
- **DWPD** is full-capacity drive writes per day over the warranty.
- Conversion ([Kingston](https://www.kingston.com/us/ssd/dwpd); [rapidtoolset](https://rapidtoolset.com/en/tool/dwpd-ssd-calculator)):

```
TBW  = DWPD × Capacity_TB × 365 × WarrantyYears
DWPD = TBW / (Capacity_TB × 365 × WarrantyYears)
e.g.  1 DWPD, 3.84 TB, 5 y  → 7,008 TBW       0.3 DWPD, 1 TB, 5 y → 547.5 TBW
```

- **Write amplification factor (WAF)** = NAND bytes written / host bytes written. It rises with random small writes, a full drive, and little over-provisioning (OP). It falls with sequential, aligned, large writes, TRIM/deallocate and more OP ([AnandTech on endurance math](https://at-web1.www.anandtech.com/show/7947/micron-m500-dc-480gb-800gb-review/3)). Rated TBW ≈ (P/E cycles × raw NAND capacity) / WAF_rated.
- **Typical P/E ratings:**

| NAND | P/E cycles |
|---|---|
| SLC | 50k–100k ([SNU course notes](https://csl.snu.ac.kr/courses/4190.568/2026-1/8-flash.pdf)) |
| MLC | 1.5k–5k |
| TLC | ~1k–3k (3k is a classic enterprise TLC figure) |
| QLC | ~1k or less ([Synology](https://blog.synology.com/tlc-vs-qlc-ssds-what-are-the-differences); [AnandTech](https://www.anandtech.com/show/12744)) |

- **Typical ratings (vendor datasheets, rounded):**
  - Consumer TLC: ~600 TBW per TB over 5 y (≈ 0.33 DWPD), e.g. Samsung 990 PRO 1 TB = 600 TBW **(unverified)**.
  - Consumer QLC: ~200–360 TBW per TB (≈ 0.1–0.2 DWPD).
  - Datacenter read-intensive TLC/QLC: 0.3–1 DWPD.
  - Mixed-use: 3 DWPD.
  - Write-intensive and SLC/SCM: 10–100 DWPD (Optane P5800X ≈ 100 DWPD).

### 3.3 Daily write budget (governor input)

```
rated_TBW         = known rating  OR  DWPD × cap_TB × 365 × years
                    OR fallback 200 TB per TB of capacity (conservative consumer/QLC)
used_frac         = max(PercentageUsed/100, host_bytes_written/rated_TBW)   # pessimistic
remaining_bytes   = rated_TBW × (1 − used_frac)
remaining_days    = max(1, target_service_days − days_in_service)           # default target: 5 y from first use, or configured
daily_budget      = remaining_bytes / remaining_days
ddram_daily_cap   = share × daily_budget − other_writers_today               # share default 0.5 on shared drives, 0.9 on dedicated
```

- **Calibrate WAF online:** when Percentage Used ticks from p to p+1, compute `bytes_per_percent = Δhost_bytes / 1`. Projected life is then `100 × bytes_per_percent`. On OCP drives, use PMUW/host bytes directly.
- **Measure other writers** as the delta of Data Units Written, or of `/sys/block/<dev>/stat` sectors written, minus ddramnvme's own writes.

---

## 4. Performance characteristics for a RAM-extension tier

### 4.1 Latency ladder (4 KiB random read, QD1 unless noted)

| Tier | Typical latency | Source |
|---|---|---|
| DRAM (local DDR4/DDR5) | ~80–120 ns load-to-use | Commonly cited; [Leipzig 5-minute-rule paper](https://old.dbs.uni-leipzig.de/file/adms2017_5minuterule.pdf) |
| CXL-attached DRAM | roughly +100–150 ns over local DRAM **(unverified, varies by platform)** | |
| Optane P5800X (3D XPoint, discontinued) | **< 6 µs at the 99th percentile, ~5 µs typical** | [StorageReview](https://www.storagereview.com/review/intel-optane-ssd-p5800x-review), [HotHardware](https://hothardware.com/reviews/intel-optane-ssd-p5800x-for-workstations-review?page=1) |
| Kioxia XL-FLASH (SLC BiCS, 4 KiB page) | **< 5 µs** raw array read, ~10× faster than TLC; drive level in the low tens of µs | [Kioxia](https://kioxia.com/en-jp/business/memory/xlflash.html), [Computer Weekly](https://www.computerweekly.com/news/252506903/Koxia-revisits-SLC-flash-to-power-FL6-and-rival-Optane) |
| Consumer Gen4 TLC (e.g. 990 PRO) | ~35–60 µs mean (≈ 24–27k IOPS at QD1) | [storedbits](https://storedbits.com/samsung-980-pro-vs-990-pro/), [LaptopMedia](https://laptopmedia.com/review/samsung-990-pro-quick-review-the-fastest-gen-4-drive-alive/p2/) |
| Enterprise Gen4 TLC (e.g. Micron 7450) | ~80 µs typical read; ms-class 99.9999 % tail under mixed load | [reseller copy of Micron spec](https://us.stockinthechannel.com/Product/Micron-7450-PRO-U-3-1-92-TB-PCI-Express-4-0-3D-TLC-NAND-NVMe/48370214) |
| Consumer QLC / DRAM-less | 80–150+ µs; much worse once the SLC cache is exhausted or the drive is throttling **(unverified)** | |

- **The gap:** a NAND page fault costs roughly 300–1000× a DRAM access. Even SCM-class media is ~50×.
- **Consequence:** the tier works only if the hot set stays in DRAM and misses are hidden by prefetch and concurrency. VMware's vSphere 9 NVMe memory tiering takes the same approach with a software-managed DRAM→NVMe hierarchy ([Virtualization Review](https://virtualizationreview.com/articles/2026/07/29/navigating-the-dram-crisis-memory-tiering-in-vsphere-9.aspx)).
- **Write latency** on enterprise drives with PLP is low (~10–20 µs) because writes are acknowledged from capacitor-protected DRAM. Consumer drives without PLP need FLUSH for durability, which can take milliseconds.

### 4.2 Throughput and queue depth

- **PCIe ceilings for an x4 link** (128b/130b encoding; [Techgage](https://techgage.com/?p=51672)):

| Generation | Rate | x4 ceiling | Real drives |
|---|---|---|---|
| Gen3 | 8 GT/s | ~3.94 GB/s | |
| Gen4 | 16 GT/s | ~7.88 GB/s | ~7.0–7.5 GB/s |
| Gen5 | 32 GT/s | ~15.75 GB/s | ~14 GB/s |

- **Random 4K IOPS needs high QD:**
  - QD1 gives ~20–30k IOPS (~100 MB/s).
  - Enterprise Gen4 reaches ~0.8–1.1 M IOPS at QD≥128 (Micron 7450: ~800k for 1.92 TB).
  - Gen5 enterprise claims ~2.5–3.3 M IOPS.
- **Little's law:** to move 4 GB/s of 4 KiB pages at 80 µs you need ≈ 1M IOPS × 80 µs ≈ **80 outstanding I/Os**. Hence the QD≥32 per device requirement.

### 4.3 Thermal throttling

- All M.2 vendors throttle once a temperature limit is crossed. Without a heatsink, throttling under sustained load is common. The Kingston NV3 throttled after ~40 s of load, with the sensor reading 71 °C while the package surface was 86 °C ([TechPowerUp NV3](https://techpowerup.com/review/kingston-nv3/8.html); [TechPowerUp ADATA Legend 960](https://www.techpowerup.com/review/adata-legend-960-1-tb-m-2-nvme-ssd/7.html)).
- Sustained sequential writes throttle hardest ([AnandTech SM951](https://at-web1.www.anandtech.com/show/9396/samsung-sm951-nvme-256gb-pcie-ssd-review/2)).
- There is no universal threshold. Use the drive's own WCTEMP/CCTEMP, the TMT1/TMT2 counters and C0h byte 97.

### 4.4 Why NAND cannot simply be used as DRAM

- **Cell physics:**
  - DRAM stores charge on a capacitor behind one transistor (1T1C). It is volatile, refreshed every ~64 ms, byte-addressable through the memory controller, and has essentially unlimited write endurance.
  - NAND stores charge in a charge-trap or floating gate. Programming and erasing work by high-voltage tunnelling, which physically wears the oxide. That gives finite P/E endurance (§3.2) and retention that degrades with wear and temperature (JESD218).
- **Granularity and erase-before-write:**
  - NAND reads and programs whole pages (4–16 KiB) and erases whole blocks (hundreds to 1.5k pages, i.e. MBs). A page cannot be overwritten in place ([USPTO 8185685 "NAND flash module replacement for DRAM"](https://patents.google.com/patent/US8185685) states NAND "cannot directly replace DRAM or any other devices that allow byte or word level random access").
  - So an FTL with remapping and garbage collection is mandatory, and that is where WAF comes from.
- **Timing:** array read tR ≈ 25–100 µs, program ≈ 0.5–1.5 ms, erase ≈ 3–10 ms (e.g. 1y-nm MLC: read 45 µs / program 1350 µs / erase 4 ms per the search results above; [FMS 2017](https://old.flashmemorysummit.com/English/Collaterals/Proceedings/2017/20170807_PreConfH_Pletka.pdf)). DRAM is ~3 orders of magnitude faster.
- **Same fabs, different devices:**
  - Both DRAM and NAND are made with photolithography, including ASML scanners. DRAM has used **EUV** layers in production since 2020 (Samsung) and 2021 (SK hynix).
  - 3D NAND scales vertically with layer stacking and high-aspect-ratio etch, mostly on DUV, with no near-term EUV plans ([TrendForce](https://www.trendforce.com/news/2025/03/11/news-micron-pioneers-1%CE%B3-ddr5-samples-with-reduced-euv-use-but-future-risks-loom/), [Asia Economy](https://cm.asiae.co.kr/en/article/2021072210554440540), [bits&chips](https://bits-chips.com/?p=126759)).
  - Sharing tools does not make the cells interchangeable. A NAND die cannot be "reused" as DRAM: the cell, sense amplifiers, array architecture and interface (ONFI/Toggle vs DDR) are fundamentally different.
  - **The realistic path is software tiering (ddramnvme) or purpose-built hybrid hardware.**
- **Hardware analogues:**
  - **Samsung CMM-H** (formerly "Memory-Semantic SSD", shown at FMS'22): a CXL Type-3 device in which an FPGA controller fronts NAND with an internal DRAM cache. It exposes byte-addressable `cxl.mem`, prefetches and keeps hot pages in DRAM, and is transparent to the host. It is excellent on cache hits and limited on misses ([Samsung tech blog](https://semiconductor.samsung.com/news-events/tech-blog/samsung-cxl-solutions-cmm-h/), [whitepaper](https://download.semiconductor.samsung.com/resources/white-paper/CMM-H_Whitepaper_10149503034923.pdf), [arXiv 2503.22017](https://arxiv.org/html/2503.22017v1)). This is essentially ddramnvme's design implemented in a device.
  - **CXL DRAM expanders**, e.g. Samsung CMM-D 128–256 GB modules ([Guru3D](https://www.guru3d.com/story/samsung-production-of-256gb-cxl-20-memory-expansion-modules-in-2024/)).
  - **Kioxia XL-FLASH** (low-latency SLC) and the discontinued **Intel Optane**.

---

## 5. Reading SMART/health programmatically (no smartctl)

### 5.1 Linux

- **Admin passthrough:** open the **controller character device** `/dev/nvmeX`, or the namespace `/dev/nvmeXnY`. `ioctl(fd, NVME_IOCTL_ADMIN_CMD, &cmd)` with `NVME_IOCTL_ADMIN_CMD = _IOWR('N', 0x41, struct nvme_admin_cmd)`, where `nvme_admin_cmd` is a typedef of `struct nvme_passthru_cmd` ([uapi `nvme_ioctl.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/nvme_ioctl.h)):

```c
struct nvme_passthru_cmd {      // 72 bytes
  __u8  opcode;   __u8 flags;  __u16 rsvd1;
  __u32 nsid;     __u32 cdw2;  __u32 cdw3;
  __u64 metadata; __u64 addr;  // addr = user buffer pointer
  __u32 metadata_len; __u32 data_len;
  __u32 cdw10, cdw11, cdw12, cdw13, cdw14, cdw15;
  __u32 timeout_ms; __u32 result;
};
```

- **Get SMART log:**
  - `opcode = 0x02`, `nsid = 0xFFFFFFFF`, `addr = buf`, `data_len = 512`.
  - `cdw10 = (NUMDL << 16) | (RAE << 15) | (LSP << 8) | LID`, with NUMD a 0-based dword count, so 512 B → 127. That makes **`cdw10 = 0x007F0002`**.
  - `cdw11 = NUMDU | (LSI << 16) = 0`; `cdw12/13` = log page offset (0); `cdw14 = UUID index | (CSI << 24) = 0`.
  - Field shifts come from libnvme [`ioctl.h`](https://github.com/linux-nvme/libnvme/blob/master/src/nvme/ioctl.h) (`NVME_LOG_CDW10_LID_SHIFT=0, LSP=8, RAE=15, NUMDL=16`).
  - Return value: 0 = OK, >0 = NVMe status, <0 = errno.
  - The same call reads OCP C0h (`cdw10 = 0x007F00C0`) and Identify (opcode 0x06, CNS=1 in cdw10, 4096 B).
- **Privilege:** Get Log Page requires **CAP_SYS_ADMIN**. Unprivileged admin passthrough is limited to some Identify CNS values (`nvme_admin_cmd_allowed()` in [`drivers/nvme/host/ioctl.c`](https://github.com/torvalds/linux/blob/master/drivers/nvme/host/ioctl.c)).
- **Unprivileged fallbacks:**
  - `/sys/class/nvme/nvmeX/{model,serial,firmware_rev,transport,address,state,numa_node,queue_count}` ([`sysfs.c`](https://github.com/torvalds/linux/blob/master/drivers/nvme/host/sysfs.c)).
  - hwmon (`CONFIG_NVME_HWMON`, [`hwmon.c`](https://github.com/torvalds/linux/blob/master/drivers/nvme/host/hwmon.c)) at `/sys/class/nvme/nvmeX/hwmon*/`:
    - `temp1_input` = composite temperature in m°C, read from the SMART log by the kernel
    - `temp2..9_input` = sensors 1–8
    - `temp1_alarm` = Critical Warning bit 1
    - `temp1_crit` = CCTEMP
    - `temp1_max`/`min` = Temperature Threshold feature
  - `/sys/block/nvmeXnY/stat` field 7 = sectors written (512 B units, kernel-side accounting), which is a privilege-free write meter for the governor.
  - `/sys/block/nvmeXnY/queue/{logical_block_size,physical_block_size,optimal_io_size,max_hw_sectors_kb,rotational}`.
- **Path → device:**
  1. `stat(path).st_dev` → `major:minor`.
  2. `readlink /sys/dev/block/MAJ:MIN`.
  3. If it is a partition (`partition` file present), take the parent directory.
  4. For dm/md/LVM, recurse through `/sys/block/<dev>/slaves/*`. This can yield multiple devices; take the worst health.
  5. `/sys/block/nvmeXnY/device` → the controller `nvmeX`; open `/dev/nvmeX`.
  6. Alignment: on Linux 6.1+, `statx(STATX_DIOALIGN)` gives the O_DIRECT memory and offset alignment.

### 5.2 Windows (no admin passthrough needed for log pages)

Source: Microsoft, [Working with NVMe drives](https://github.com/MicrosoftDocs/win32/blob/docs/desktop-src/FileIO/working-with-nvme-devices.md) (published at learn.microsoft.com/windows/win32/fileio/working-with-nvme-devices). Cross-checked with smartmontools [`os_win32.cpp`](https://github.com/smartmontools/smartmontools/blob/master/smartmontools/os_win32.cpp) `win10_nvme_device::nvme_storage_query_property`. StorNVMe documents Get Log Page as served through `IOCTL_STORAGE_QUERY_PROPERTY` ([StorNVMe command set support](https://github.com/MicrosoftDocs/windows-driver-docs/blob/staging/windows-driver-docs-pr/storage/stornvme-command-set-support.md)).

```c
typedef struct _STORAGE_PROPERTY_QUERY {        // input header
  STORAGE_PROPERTY_ID PropertyId;   // StorageDeviceProtocolSpecificProperty (=50) for log pages
  STORAGE_QUERY_TYPE  QueryType;    // PropertyStandardQuery (=0)
  UCHAR AdditionalParameters[1];    // <- STORAGE_PROTOCOL_SPECIFIC_DATA starts here (offset 8)
} STORAGE_PROPERTY_QUERY;

typedef struct _STORAGE_PROTOCOL_SPECIFIC_DATA { // 40 bytes
  STORAGE_PROTOCOL_TYPE ProtocolType;   // ProtocolTypeNvme (=3)
  ULONG DataType;                       // NVMeDataTypeLogPage (=2)  [Identify=1, Feature=3]
  ULONG ProtocolDataRequestValue;       // NVME_LOG_PAGE_HEALTH_INFO (=0x02) – the LID
  ULONG ProtocolDataRequestSubValue;    // log page offset low 32 (newer drivers → CDW12)
  ULONG ProtocolDataOffset;             // = sizeof(STORAGE_PROTOCOL_SPECIFIC_DATA) (40)
  ULONG ProtocolDataLength;             // = 512 (sizeof NVME_HEALTH_INFO_LOG); must be >= 512
  ULONG FixedProtocolReturnData;        // out: completion DW0
  ULONG Reserved[3];                    // _EXT variant: SubValue2 (offset hi), SubValue3 (LSI→CDW11), SubValue4 (RAE/LSP)
} STORAGE_PROTOCOL_SPECIFIC_DATA;
// Output: STORAGE_PROTOCOL_DATA_DESCRIPTOR { DWORD Version; DWORD Size; STORAGE_PROTOCOL_SPECIFIC_DATA ProtocolSpecificData; }
```

Flow:
1. `bufLen = FIELD_OFFSET(STORAGE_PROPERTY_QUERY, AdditionalParameters) + sizeof(STORAGE_PROTOCOL_SPECIFIC_DATA) + NVME_MAX_LOG_SIZE` (or + 512). Allocate and zero the buffer.
2. Fill the query and protocol data as above. The data lands at `buffer + 8 + 40`.
3. `DeviceIoControl(hDisk, IOCTL_STORAGE_QUERY_PROPERTY /*0x002D1400*/, buf, bufLen, buf, bufLen, &ret, NULL)`.
4. Validate the result:
   - `desc->Version == desc->Size == sizeof(STORAGE_PROTOCOL_DATA_DESCRIPTOR)`
   - `ProtocolDataOffset >= 40`
   - `ProtocolDataLength >= 512`
5. Parse the log at `(BYTE*)&desc->ProtocolSpecificData + ProtocolDataOffset`. MS sample: temperature = `(T[1]<<8 | T[0]) - 273`.
6. Other queries:
   - Identify Controller uses `StorageAdapterProtocolSpecificProperty` (49), `NVMeDataTypeIdentify`, and `RequestValue = NVME_IDENTIFY_CNS_CONTROLLER`.
   - Vendor log pages (e.g. OCP C0h) go through the same query in 512 B chunks.
   - Temperature alone is available through `StorageDeviceTemperatureProperty` → `STORAGE_TEMPERATURE_DATA_DESCRIPTOR` (°C, signed, with thresholds).

Notes:
- **Inbox StorNVMe only:** vendor NVMe drivers and RAID/VMD stacks may not implement the protocol-specific query. Fall back to "health unknown".
- **Access rights:** opening `\\.\PhysicalDriveN` with `dwDesiredAccess = 0` is normally enough for `IOCTL_STORAGE_QUERY_PROPERTY` (FILE_ANY_ACCESS) **(unverified for protocol-specific queries; test as non-admin)**.
- **Rust:** the `windows` crate exposes `Win32::System::Ioctl::{STORAGE_PROPERTY_QUERY, STORAGE_PROTOCOL_SPECIFIC_DATA, ...}` and `Win32::Storage::Nvme::NVME_HEALTH_INFO_LOG`.
- **Path → device:**
  1. `GetVolumePathNameW(path)`.
  2. `GetVolumeNameForVolumeMountPointW`.
  3. `CreateFileW("\\\\?\\Volume{GUID}")`, without the trailing backslash.
  4. `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` → `DiskNumber`(s). More than one extent means a spanned volume or Storage Spaces; take the worst health.
  5. `\\.\PhysicalDrive<N>`. Confirm NVMe with `StorageDeviceProperty` → `STORAGE_DEVICE_DESCRIPTOR.BusType == BusTypeNvme (17)`.
- **Unbuffered I/O:** `FILE_FLAG_NO_BUFFERING` (sector-aligned offsets, sizes and buffers) plus overlapped I/O or IoRing for QD.

### 5.3 macOS (IOKit NVMeSMARTLib plug-in)

Source: smartmontools [`os_darwin.cpp`](https://github.com/smartmontools/smartmontools/blob/master/smartmontools/os_darwin.cpp) and [`os_darwin.h`](https://github.com/smartmontools/smartmontools/blob/master/smartmontools/os_darwin.h), which mirror Apple's `NVMeSMARTLibExternal.h`.

```c
#define kIOPropertyNVMeSMARTCapableKey "NVMe SMART Capable"
kIONVMeSMARTUserClientTypeID = AA0FA6F9-C2D6-457F-B10B-59A13253292F
kIONVMeSMARTInterfaceID      = CCD1DB19-FD9A-4DAF-BF95-12454B230AB6
typedef struct IONVMeSMARTInterface {
  IUNKNOWN_C_GUTS; UInt16 version, revision;
  IOReturn (*SMARTReadData)(void *self, struct nvme_smart_log *out);          // 512-B log 02h
  IOReturn (*GetIdentifyData)(void *self, struct nvme_id_ctrl *out, unsigned ns);
  UInt64 reserved0, reserved1;
  IOReturn (*GetLogPage)(void *self, void *buf, unsigned lid, unsigned numDWordsZeroBased);
  /* reserved2..19 */
} IONVMeSMARTInterface;
```

Call sequence:
1. Get the BSD name: `statfs(path)` → `f_mntfromname`, e.g. `/dev/disk3s1s1`, then strip `/dev/`.
2. `IOServiceGetMatchingService(kIOMainPortDefault, IOBSDNameMatching(kIOMainPortDefault, 0, "disk3s1s1"))`.
3. Walk up `IORegistryEntryGetParentEntry(entry, kIOServicePlane, &parent)` until `IORegistryEntryCreateCFProperty(entry, CFSTR("NVMe SMART Capable"), …)` is non-NULL.
   - For APFS the walk goes APFS volume → `AppleAPFSContainerScheme` → physical-store partition → `IOMedia` disk0 → NVMe block-storage device / controller.
   - No such node means a USB/Thunderbolt bridge, RAID or virtual disk, so treat health as unknown.
4. `IOCreatePlugInInterfaceForService(node, kIONVMeSMARTUserClientTypeID, kIOCFPlugInInterfaceID, &plugin, &score)`.
5. `(*plugin)->QueryInterface(plugin, CFUUIDGetUUIDBytes(kIONVMeSMARTInterfaceID), (LPVOID*)&smart)`.
6. `(*smart)->SMARTReadData(smart, &log)`, or `(*smart)->GetLogPage(smart, buf, 0x02, 127)` (`numDWords` is 0-based, so 512/4−1). `GetIdentifyData(smart, &idctrl, 0)` returns WCTEMP/CCTEMP.
7. `(*smart)->Release(smart); IODestroyPlugInInterface(plugin); IOObjectRelease(node)`.

Notes:
- Only Identify and Get Log Page are exposed; there is no arbitrary admin passthrough.
- `kIOMasterPortDefault` is deprecated from macOS 12; use `kIOMainPortDefault` (or `0`).
- smartmontools uses this path on Apple-silicon internal SSDs as well **(unverified for every model; a sandboxed app may be denied)**.
- **I/O:** there is no `O_DIRECT`. Use `fcntl(fd, F_NOCACHE, 1)` to bypass the unified buffer cache, and `F_FULLFSYNC` when durability matters.

---

## 6. Example outputs and interpretation

**A. Consumer: Samsung 970 EVO Plus 250 GB.** smartctl 7.3 JSON from the [prometheus-community/smartctl_exporter test data](https://github.com/prometheus-community/smartctl_exporter/blob/master/testdata/nvme-null-Samsung_SSD_970_EVO_Plus_250GB-nvme0.json), `nvme_smart_health_information_log`:

```
critical_warning 0  temperature 35  available_spare 100  available_spare_threshold 10
percentage_used 0   data_units_read 2,025,034  data_units_written 1,969,459
host_reads 13,927,240  host_writes 88,382,369  controller_busy_time 892
power_cycles 16  power_on_hours 3149  unsafe_shutdowns 12  media_errors 0
num_err_log_entries 22  warning_temp_time 0  critical_comp_time 0  temperature_sensors [35, 39]
```

- **Host writes so far:** 1,969,459 × 512,000 B = **1.01 TB**. The average write command is 1.01 TB / 88.4 M ≈ 11 KiB.
- **Budget:** the rating is 150 TBW over 5 y **(unverified vendor rating)**, so ~149 TB remains. Spread over the remaining warranty that is ≈ **80 GB/day**, so a 50 % governor share allows ~40 GB/day.
- **Other fields:** 22 error-log entries with 0 media errors are usually benign (e.g. rejected unsupported commands). Unsafe shutdowns can come from OS shutdown paths that skip CC.SHN, as discussed in the [EndeavourOS forum](https://forum.endeavouros.com/t/unsafe-shutdown-errors-ssd/6317) and [FreeBSD bug 211852](https://bugs.freebsd.org/bugzilla/show_bug.cgi?id=211852).

**B. Datacenter: Samsung PM9A3 3.84 TB (MZQL23T8HCLS).** Same source, [file](https://github.com/prometheus-community/smartctl_exporter/blob/master/testdata/nvme-null-SAMSUNG_MZQL23T8HCLS-00A07-nvme9.json):

```
critical_warning 0  temperature 22  available_spare 100  percentage_used 3
data_units_read 62,679,808  data_units_written 639,454,618  host_writes 16,276,566,236
controller_busy_time 5203  power_cycles 4  power_on_hours 4369  unsafe_shutdowns 1  media_errors 0
```

- **Host writes so far:** 639,454,618 × 512,000 B = **327.4 TB** in 4369 h (182 days), i.e. **1.8 TB/day ≈ 0.47 DWPD**. The average write is ≈ 20 KiB.
- **Against the rating:** at 1 DWPD × 5 y, rated TBW = 3.84 × 365 × 5 = 7,008 TB. Host writes are 4.7 % of that, while the drive reports 3 % used. The vendor's wear estimate is therefore below linear TBW consumption: the workload has low WAF, and/or the rating is conservative.
- **Projection:** at the current rate, 3 % per 182 days means 100 % after ≈ 16.6 years. The governor would see a large daily budget: (7,008 × 0.97 TB) / (1,826 − 182 days) ≈ 4.1 TB/day.

**C. Older capture: smartctl 6.6 on Proxmox, anonymised model.** From the [proxmox/pve-storage test fixture `nvme0n1_smart`](https://github.com/proxmox/pve-storage/blob/master/src/test/disk_tests/nvme_smart/nvme0n1_smart):

```
SMART/Health Information (NVMe Log 0x02, NSID 0xffffffff)
Critical Warning:                   0x00
Temperature:                        32 Celsius
Available Spare:                    100%
Available Spare Threshold:          10%
Percentage Used:                    31%
Data Units Read:                    1,299,288 [665 GB]
Data Units Written:                 5,592,478 [2.86 TB]
Host Read Commands:                 30,360,807
Host Write Commands:                470,356,196
Controller Busy Time:               12
Power Cycles:                       98
Power On Hours:                     687
Unsafe Shutdowns:                   21
Media and Data Integrity Errors:    0
Error Information Log Entries:      0
```

- **The mismatch:** 31 % used after only 2.86 TB of host writes, with an average write of ≈ 6 KiB (small random writes on a VM host). Extrapolated linearly, the drive's life is ≈ 9 TB of host writes.
- **The lesson:** Percentage Used, not host TBW, is the authoritative wear signal. Small or unaligned writes can raise effective WAF so far that a TBW-only governor would over-spend the budget by an order of magnitude. Hence `used_frac = max(...)` in §3.3.

Another real-world caveat: some drives occasionally return an all-zero log, with temperature reading −273 °C ([nvme-cli #1729](https://github.com/linux-nvme/nvme-cli/issues/1729)). Treat an all-zero log as **invalid**, not as healthy.

---

## 7. Requirements derived for ddramnvme

**Health gating**

1. **R-H1.** Read SMART log 02h, plus Identify Controller for WCTEMP/CCTEMP, at attach time and then every 60 s while active (at most once per 10 s). Use the per-OS mechanisms in §5; do not depend on smartctl. If health cannot be read (no privilege, unsupported driver, bridge), run in **degraded-unknown mode**: conservative default budget (R-W3) and temperature only from hwmon or `StorageDeviceTemperatureProperty` if available.
2. **R-H2.** **Refuse** to place new pages on a device whose Critical Warning has bit 0 (spare), bit 2 (reliability), bit 3 (read-only) or bit 4 (PLP backup failed) set, or whose EG warning summary has bits 0, 2 or 3 set. If such a bit appears at runtime, stop writes, drain dirty pages to DRAM or another device, and keep serving reads of existing pages for as long as possible.
3. **R-H3.** Also refuse or retire a device when `AvailableSpare < AvailableSpareThreshold`, when `PercentageUsed >= 100` (configurable, default 95 for a warning and 100 to stop writing), or when `MediaErrors` increases at runtime. Any increase means retire, because a RAM tier cannot tolerate silent data loss.
4. **R-H4.** Validate every log read. Reject all-zero logs, temperatures of 0 K or outside 200–400 K, and counters that decrease. On OCP drives (C0h GUID matches), also use PMUW for WAF and Capacitor Health / Incomplete Shutdowns for PLP status.

**Write governor (endurance)**

5. **R-W1.** Compute `daily_budget` as in §3.3 using `used_frac = max(PercentageUsed/100, host_bytes/rated_TBW)`. Inputs come from user config (capacity, DWPD or TBW, warranty), the OCP Endurance Estimate, or the fallback.
6. **R-W2.** Cap ddramnvme's own writes to **share × daily_budget** (default share 0.5; 0.9 when the device is configured as dedicated). Enforce with a token bucket refilled continuously at `cap/86400` B/s and a burst allowance of at most 10 % of the daily cap. Measure other writers through Δ Data Units Written or `/sys/block/*/stat`.
7. **R-W3.** With an unknown rating, assume **200 TBW per TB of capacity over 5 years** (≈ 0.11 DWPD) until a Percentage Used increment allows WAF calibration (`bytes_per_percent`).
8. **R-W4.** When the token bucket is empty, the tier degrades gracefully: stop writing back clean-able pages, evict only clean pages, and apply memory-pressure backpressure to the caller (an allocation failure or blocking policy, configurable). It must never write regardless of the budget.
9. **R-W5.** **Never write back read-mostly or clean data.** A page whose up-to-date copy already exists on the device is dropped from DRAM without I/O. Track a per-page dirty bit, and dirty-generation counters if needed.
10. **R-W6.** Coalesce writes by staging dirty pages and writing in large, sequential, physically-contiguous extents (≥ 128 KiB, ideally aligned to `optimal_io_size`, or 1 MiB if that is unknown). Use a log-structured slot allocator to keep device WAF near 1. Issue TRIM/deallocate (`fallocate(PUNCH_HOLE)`, `FSCTL_FILE_LEVEL_TRIM`, `F_PUNCHHOLE`) on freed slots.
11. **R-W7.** Don't rewrite hot-dirty pages repeatedly. Apply write-back hysteresis: a page must stay dirty for a minimum interval or be evicted before it is written. Account for each write's bytes against the budget before issuing it.

**Thermal**

12. **R-T1.** If the composite temperature is ≥ WCTEMP (or ≥ 70 °C when WCTEMP = 0), or `TMT1/TMT2` transition counters increase, or OCP C0h byte 97 ≠ 0, cut the write rate by 50 % and the prefetch depth by 50 %. If the temperature is ≥ CCTEMP − 5 K, or Critical Warning bit 1 is set, pause writes and prefetch until it drops below WCTEMP − 5 K (hysteresis).
13. **R-T2.** Treat a sustained jump in observed latency (p99 > 4× baseline for 10 s) as possible throttling, even when the temperature reads normal. Sensors can under-read the package by 15 °C.

**I/O path and performance**

14. **R-P1.** Use direct I/O for the tier on every OS: Linux `O_DIRECT` (alignment from `STATX_DIOALIGN`, else `logical_block_size`), Windows `FILE_FLAG_NO_BUFFERING` (+ `FILE_FLAG_OVERLAPPED`), macOS `F_NOCACHE`. All offsets, lengths and buffers must be 4 KiB-aligned; the tier page size is ≥ 4 KiB.
15. **R-P2.** Keep **QD ≥ 32 per device**, and target 64–128 for throughput (Little's law, §4.2). Use io_uring on Linux (fallback: thread pool with `pread`/`pwrite`), IOCP/IoRing on Windows, and a GCD or thread pool on macOS. Reserve a small high-priority queue share for demand faults so prefetch never delays them.
16. **R-P3.** Assume a miss costs 50–150 µs on TLC and ≥ 1 ms at the tail. Size prefetch so that prefetch depth ≥ expected stall time × fault rate. Expose p50/p99/p99.9 fault latency metrics. Where the drive supports OCP C3h, optionally log it next to our own metrics.
17. **R-P4.** Detect the device class at attach time (Identify: model, VWC, OCP C0h presence, PLP via C0h capacitor health) and pick defaults accordingly: consumer means a conservative budget and a high flush cost; datacenter with PLP means a larger budget and cheap writes. Do not rely on FLUSH being fast without PLP.

**Correctness and durability**

18. **R-C1.** The tier is a cache of volatile memory, so durability across power loss is **not** required by default and the backing file is treated as garbage after an unclean start. Still checksum each slot (e.g. CRC32C or xxh3) and verify on read. On a mismatch, report a hard error to the caller and never return corrupt data.
19. **R-C2.** Map the backing file to its physical device(s) with the per-OS procedures in §5. If it spans multiple devices (LVM, Storage Spaces, APFS Fusion), apply the most restrictive health and budget across them. Refuse to back the tier with network filesystems or rotational media unless explicitly overridden. Warn when the file sits on a QLC or DRAM-less drive, detected by low rated TBW or by user config.
20. **R-C3.** Every 02h/C0h parser must be table-driven from the offsets in §1.1/§2.2, use u128 counters, and be tested against the example dumps in §6 and the smartctl_exporter test data corpus.
