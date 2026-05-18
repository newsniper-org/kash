---
name: kash — virtual environment (venv) system (committed)
description: Soft scoped venv with fine-grained capabilities + coarse profile aliases; config files non-executable; inspired by Python venv + capability-based security
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash venv 시스템 확정 사항. (관련: project_shell_namespace.md, project_kash_security_policy.md, project_shell_modes.md)

## 목적

Shell context isolation — environment / PATH / namespace import / capability를 한 단위로 묶는 declarative block. Python venv의 prefix-override 모델 + capability-based security 정신.

## Soft (advisory) 모델 — v1

Shell-level check만 — 외부 명령이 spawn되면 capability 검사가 advisory에 그침. cross-platform, 학습 용이.

**Hard venv** (Linux namespaces + seccomp + cgroup으로 진짜 sandbox)는 **v2+로 영구 연기**. shell이 통제할 수 있는 entry/exit 시점에 check + bookkeeping만 한다.

## 문법 — declarative block

```sh
venv myproj {
    capabilities {
        profile basic       # coarse profile alias
        + fs-read           # explicit fine-grained add
        - exec-spawn        # explicit fine-grained remove (from profile)
        allow-cmd { ls, cat, grep }
    }
    env {
        PYTHONHOME = ~/proj/.venv
        PATH-prepend ~/proj/.venv/bin
    }
    imports {
        use namespace proj.utils
    }
    body {
        # 이 안의 statements는 venv scope 내에서 실행
        run-tests
    }
}
```

block은 `venv NAME { … }` 형태 — `mode`, `namespace`처럼 head-position bare-word lookahead 키워드.

## Fine-grained capabilities + coarse profiles

**Fine-grained primitive capabilities** (개별 명시 가능):
- `fs-read` / `fs-write` / `fs-exec` / `fs-create` / `fs-delete`
- `exec-spawn` (외부 명령 실행)
- `net-tcp-client` / `net-tcp-server` / `net-udp` / `net-dns`
- `env-mutate` / `env-read-secret`
- `signal-send` / `proc-fork`
- `clock-realtime` / `clock-set`

**Coarse profiles** — fine-grained의 명명된 합:
- `none` — 아무 capability 없음 (deny-all)
- `basic` — `fs-read`, `exec-spawn`(allow-cmd 한정), `env-read-secret`
- `dev` — `basic` + `fs-write`, `fs-create`, `proc-fork`, `clock-realtime`
- `network` — `dev` + `net-tcp-client`, `net-dns`
- `server` — `network` + `net-tcp-server`, `net-udp`
- `full` — 모든 capability (kash 외부 명령 호출 시 default 와 동일, advisory하니 trust user)

block 안에서 `profile X` + `+ Y` / `- Z`로 add/remove 가능.

## Config 파일 — 데이터 only (필수)

**보안 요구**: venv 설정을 담은 외부 파일은 *절대로 실행 가능한 형식이어서는 안 됨*. 즉:
- ❌ kash script 파일 (`.kash`)을 source해서 venv 정의하는 패턴 금지
- ❌ shell-out하는 어떤 형식도 금지
- ✅ TOML, JSON, YAML 같은 *순수 데이터* 형식만 허용

**채택 형식**: TOML (Rust 생태계 표준 + 사람-친화적 + key/value/table 구조가 capabilities/env/imports에 잘 맞음).

```toml
# .venv/profile.toml
[capabilities]
profile = "basic"
add = ["fs-write"]
remove = ["exec-spawn"]
allow-cmd = ["ls", "cat", "grep"]

[env]
PYTHONHOME = "~/proj/.venv"
"PATH-prepend" = "~/proj/.venv/bin"

[imports]
use = ["proj.utils", "proj.tools.{a,b}"]
```

block 안에서 `load-config PATH`로 reference:
```sh
venv myproj {
    load-config ./.venv/profile.toml
    body { ... }
}
```

config는 *데이터로만* 파싱 — kash가 표현식/명령 evaluation 안 함. 

## Scoped semantics — push/pop

venv block은 function frame과 동일한 push/pop semantics:
- `body { … }` 진입 시 venv frame push
  - env overlay 적용 (caller env 복사 + 변경)
  - PATH overlay 적용
  - imports 적용 (namespace import 추가)
  - capability set 활성화
- body 종료 시 frame pop
  - 모든 overlay 자동 복원
  - import 자동 제거

함수 호출처럼 *lexical* scope. venv 안에서 unbounded `mode`도 venv exit에서 복원 (mode-save stack과 통합).

## Capability check 지점

- 외부 명령 spawn 직전: `exec-spawn` + 명령이 `allow-cmd`에 있는지 검사
- 파일 open 시: `fs-read` / `fs-write` / `fs-create`
- 환경변수 modify 시: `env-mutate`
- network builtin (`tcp-connect` 등) 호출 시: `net-tcp-client` 등
- 위반 시 `KashError::CapabilityDenied` 또는 비슷 — venv 종료가 아니라 호출 실패. caller가 try/handle.

advisory model이라 "체크 없이도 동작"하는 경로는 그대로 통과 (현재 그러함). venv 안에 있을 때만 추가 check.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `venv NAME { ... }` block | × | ✓ | × | ✓ | ✓ |
| `load-config` | × | ✓ | × | ✓ | ✓ |
| Capability check | × | ✓ (advisory) | × | ✓ (advisory) | ✓ (advisory) |

`-secure` modifier와 결합: `-secure` 모드에서는 capability profile이 `basic`보다 엄격해야 함 (e.g. `exec-spawn` 기본 off).

## 구현 stages

(v.1) AST + parser `venv NAME { … }` + body의 push/pop frame
(v.2) capability profile 시스템 (built-in profiles 등록 + add/remove syntax)
(v.3) env overlay + PATH overlay
(v.4) external command capability check (`exec-spawn` + `allow-cmd`)
(v.5) TOML config 파일 loader (`load-config PATH`)
(v.6) imports 통합 (namespace import auto-apply on entry)
(v.7) fs / net capability check (file open, network builtins)

각 stage 단위 commit. (v.1)~(v.4)이 user-facing의 핵심 80%, (v.5)~(v.7)이 고급.

## How to apply

신규 declarative form 추가 시 venv block의 push/pop 의미론과 자동 정합되어야. capability profile 확장 시 fine-grained primitive에 매핑해서 정의. 외부 config 파일 형식 추가 시 *반드시 데이터-only* 보장 (실행 가능 형식 도입 금지).

## 미결

v2+:
- Hard sandbox (Linux namespaces / seccomp / cgroup) — kash 자체 process 단의 격리 도입 시점
- Persistent venv state (Python `.venv/` directory 같은) — interactive shell의 enter/exit 모델
- venv 간 inheritance — venv A가 venv B를 base로 derive
