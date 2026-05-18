---
name: lightnht 후속 최적화 후보 (미적용)
description: lightnht의 descent hot path 추가 최적화 아이디어 모음. 현재 BTreeBackend 대비 5-10배 느린 bench 결과의 개선 후보.
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
벤치 결과 (`kash-core/tests/bench_backends.rs`): `BTreeBackend` 234/384/215 ns/op vs `LightnhtBackend<QLMHasher>` 1179/1483/1073 ns/op. SwarQLM은 더 느림 (2382/2927/2174). 결정은 BTreeBackend 유지 (51b72c9 직전 commit 8d37f9a). lightnht는 학습/실험 가치.

**왜:** QLMHasher의 hash compute cost가 주범 (~280 ALU per 5-byte key, FxHash의 10배). 추가로 lightnht 자체 non-hasher overhead — depth_coord `Vec<u8>` heap alloc, hasher clone per descent, recursive descent overhead.

**How to apply:** 다음 lightnht 작업 사이클 시 아래 세 layer를 같이 적용하면 bench 결과 측정 의미 있음.

## 후보 1: hasher 교체

dev-dep로 `fxhash` / `xxhash-rust` / `ahash` 추가, bench에 `LightnhtBackend<fxhash::FxHasher64>` / `<xxh3>` / `<ahash::AHasher>` 행 추가. 추정 350-500 ns/op 수준까지 단축 가능 (BTree의 1.5-2× 정도). BTree를 *이기긴* 어려움 — lightnht의 non-hasher overhead가 남아있어서.

## 후보 2: depth_coord 3-layer 최적화 (사용자 제안)

현재 `depth_coord: Vec<u8>` — 매 descent마다 push/pop, heap alloc 가능성.

**Layer 1 (stack-bounded)**: `struct DepthCoord { data: [u8; MAX_DEPTH /* = 21 */], len: u8 }`. heap alloc 0. 가장 큰 perf gain 후보 — push/pop이 단순 byte store + len ±1.

**Layer 2 (position offset)**: 저장 시 `d_k = c_k + k`로 transform. c_k는 slot index (0-7), k는 depth (0-20). `c_k + k ≤ 27` u8 fit. 같은 slot index가 path의 위치별로 distinct 값 → avalanche 개선. zero overhead (push 시 ADD 1).

**Layer 3 (u32 byte-packing for hash input)**: hash input 단계에서 depth_coord를 4-byte chunked u32 시퀀스로 packing해서 hasher에 write. n ≤ 4면 단일 u32, n > 4면 chunked. 단 Rust의 `Hasher::write_u32` default는 byte-by-byte fallback이라 *그 자체*로는 perf 변화 거의 없음 — Layer 1과 함께 적용해야 진짜 의미. xxh3/ahash 같은 native u32-path 있는 hasher와 결합 시 더 효과.

세 layer 다 적용 + 적당한 hasher 조합으로 `Lightnht` 1179 ns → 700-900 ns 정도 기대 (추정치, 측정 필요).

## 적용 정책

위 후보들은 사용자 의향만 받은 *idea*. 실제 적용 commit은 별도. BTree가 여전히 빠를 가능성이 높음 (작은 N에서의 cache locality). lightnht의 본 목적이 학습/실험이라 적용 자체보단 *측정해서 정확한 trade-off 파악*이 더 가치 있음.
