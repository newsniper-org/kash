---
name: kash hard-sandbox sibling project (committed)
description: OS-level enforced sandbox는 kash 본체와 분리된 sibling project. 쉘 실행파일명에 `-sandbox` postfix.
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash의 soft venv (advisory) 위에 *OS-level enforced* hard sandbox를 별도 sibling project로 분리하기로 확정. (관련: project_kash_venv.md, project_shell_modes.md)

## 결정 이유

- hard sandbox 코드는 OS-specific(Linux Landlock, OpenBSD pledge/unveil, BSD Capsicum, macOS sandbox_init, Windows Job Object)이라 kash 본체보다 platform-specific bug surface가 큼.
- release cadence가 다름 — kash 본체는 언어 기능 위주, sandbox는 OS API 변동(Landlock minor version bump 등)에 따라 자주 갱신.
- 사용자가 *언제 sandbox 강제*를 선택할지 명시적이어야 — venv block을 적은 *모든* kash invocation에 자동 enforce면 portability 깨짐.

## 분리 방식

별도 binary, 같은 codebase 공유. workspace 안 새 crate `kash-sandbox` 또는 별도 repo.

## 실행파일명 convention

shell 실행파일명에 `-sandbox` postfix 추가:

| invoke | 본체 | sandbox 버전 |
|---|---|---|
| 정규 이름 | `kash` | `kash-sandbox` |
| POSIX sh 호환 | `sh` | `sh-sandbox` |
| ksh93 호환 | `ksh` / `ksh93` | `ksh-sandbox` / `ksh93-sandbox` |

argv[0] basename 검사 시 `-sandbox` 접미사 분리 → 기반 이름(kash/sh/ksh/ksh93)으로 mode dispatch + sandbox layer ON.

## Mode 무관

sandbox 활성화는 **모드와 직교**. `kash-sandbox --mode=default` / `kash-sandbox --mode=posix-strict` 모두 sandbox enforce. mode가 strict여도 sandbox는 별개 layer.

## venv 통합

kash-sandbox는 *같은* `venv NAME { … }` 문법 사용. 차이는 capability gate 효과:

- 본체 `kash`: capability check이 **advisory** (shell이 직접 부르는 syscall에서만 차단)
- `kash-sandbox`: capability check이 **enforced** (OS-level sandbox로 child process syscall까지 차단)

같은 source script가 두 binary에서 잘 작동해야 함. capability set은 source에 적힌 그대로 + sandbox binary가 OS API로 transduce.

## OS 백엔드 우선순위 + 가용성

| OS | backend | 보장 강도 |
|---|---|---|
| Linux 5.13+ | Landlock | strong (FS / exec / net) |
| OpenBSD | pledge + unveil | strong, capability model 정합 |
| FreeBSD + DragonFlyBSD | Capsicum (`cap_enter`, `cap_rights_limit`) | exec / net 강함, FS는 부분 (fd-based) |
| macOS | `sandbox-exec` wrapper 또는 `sandbox_init` (private API) | medium, fragile |
| Windows | Job Object + Restricted Token + AppContainer | medium, integrity level 기반 |
| NetBSD | 표준 없음 | NoSandbox + warning |
| 기타 | NoSandbox | warning |

NoSandbox 백엔드 — venv block 진입 시 stderr에 1회 경고 ("hard sandbox not available on this platform; capabilities remain advisory"). 종료 코드는 정상 (실패시키지 않음 — 사용자가 hardening 의도 알고 있다고 가정).

## capability → backend mapping (개략)

| kash capability | Linux | OpenBSD | BSD-Capsicum | macOS | Windows |
|---|---|---|---|---|---|
| `fs-read` X | Landlock no-read | pledge w/o `rpath` | Capsicum N/A (어색) | sandbox profile | DACL deny read |
| `fs-write` X | Landlock no-write | pledge w/o `wpath`+`cpath` | (어색) | sandbox profile | DACL deny write |
| `exec-spawn` X | Landlock no-exec / seccomp | pledge w/o `exec` | Capsicum (no exec in cap mode) | sandbox profile | Job Object no-spawn |
| `net-tcp-client` X | seccomp socket / Landlock-net (kernel 5.18+) | pledge w/o `inet` | Capsicum | sandbox profile | Firewall token |
| `allow-cmd` | path-allow via Landlock | unveil 특정 binary 만 r-x | (어색) | sandbox profile | — |

매핑이 어색한 항목은 본 platform 한정 *advisory* 표기 + 별도 경고.

## 인터페이스 안정성

`kash-sandbox`의 CLI surface는 `kash`와 **동일** — same flags, same script compatibility. 차이는 enforcement layer뿐. 사용자가 `#!/usr/bin/env kash-sandbox` shebang 한 줄로 hardened mode 진입.

## 구현 phase

1. workspace 새 crate `kash-sandbox` skeleton + `Sandbox` trait + `NoSandbox` default
2. argv[0] `-sandbox` postfix 처리 + sandbox layer hook
3. Linux Landlock backend
4. OpenBSD pledge + unveil backend
5. BSD-Capsicum backend (FreeBSD + DragonFly 공통)
6. macOS sandbox-exec wrapper backend
7. Windows Job Object backend
8. NetBSD NoSandbox + 명시적 경고
9. test matrix (CI 환경 별도)

각 phase 별도 commit 사이클. kash 본체 release cycle과 독립.

## 미결

- v2+ 통합 시점: kash-sandbox가 자체 stable 도달 후 kash workspace에 정식 편입 vs 별도 repo로 유지
- venv profile 안에서 "this profile requires sandbox" annotation: 사용자가 sandbox binary가 아닌 본체로 invoke 시 warning할 hint
- shebang `#!/usr/bin/env kash-sandbox --mode=...` flag 호환 (sandbox binary가 --mode= flag 그대로 받음)

## How to apply

- 본체 kash 안에서 hard sandbox 관련 PR 들어와도 거부. sibling project로 회부.
- 본체 kash가 새 capability primitive 추가 시 — `kash-sandbox`의 backend mapping table에도 entry 필요.
- `project_kash_venv.md`의 "Hard sandbox (Linux namespaces / seccomp / cgroup) — kash 자체 process 단의 격리 도입 시점" 항목 → "kash-sandbox sibling project로 분리"로 update.
