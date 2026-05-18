---
name: kash — module resolution convention (committed)
description: 파일 path → namespace 자동 매핑, search path 우선순위, source vs use 분리, manifest는 frontend 책임 (Slackware-style 수동 관리)
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 모듈 시스템 — Slackware-style 완전 수동 관리 기반, kash core는 *resolution + loading*만 책임. (관련: project_shell_namespace.md, project_kash_config.md)

## File path ↔ namespace 매핑 (자동)

```
<MODULE_ROOT>/foo/bar.kash         ↔  .foo.bar
<MODULE_ROOT>/foo.kash              ↔  .foo
<MODULE_ROOT>/org/example/v2/utils.kash  ↔  .org.example.v2.utils
```

규칙:
- File path segment ↔ namespace path 일대일 대응
- 파일 내용이 *자동으로* `namespace .foo.bar { ... }` 로 wrap
- 사용자가 파일에 명시할 필요 없음 (선택적 명시 가능 — 일치해야 함, 불일치 시 error)

### Single-file vs directory

```
modules/
├── foo.kash              # .foo  (init code 포함 가능)
└── foo/                  # .foo.* 하위 namespace 디렉토리
    ├── bar.kash          # .foo.bar
    └── baz.kash          # .foo.baz
```

`foo.kash` 와 `foo/` 디렉토리 공존 가능 — 다른 항목 정의.

**No `__init__.kash` magic** — Python식 init 파일 없음. 단순화.

## Search paths (우선순위)

앞이 우선 — first-found-wins:

1. **Custom**: `KASH_MODULE_PATH` env var (`:` 구분, 가장 앞에 prepend)
2. **User**: `~/.local/share/kash/modules/`
3. **System**: `/usr/local/share/kash/modules/`
4. **Built-in**: `/usr/share/kash/modules/`

```sh
KASH_MODULE_PATH=/opt/myteam/kash:./local-modules use namespace .foo.bar
# 검색: /opt/myteam/kash → ./local-modules → ~/.local/... → /usr/local/... → /usr/share/...
```

충돌 시 `warn-module-conflict` 옵션 (warn-* 패밀리 확장 — v2 또는 v1 검토). 현재는 silent first-found.

## Loading 동작 (`use namespace`)

1. Registry 확인 — `.foo.bar` 이미 load? 있으면 import만.
2. 없으면 search path에서 `foo/bar.kash` 검색.
3. 첫 발견된 file load — 내용을 `namespace .foo.bar { ... }` 로 자동 wrap (또는 명시되어 있으면 그대로).
4. Registry에 등록.
5. Use를 caller scope에 적용 (symbol import).

### Idempotency
`use namespace .foo.bar` 두 번 호출 → 두 번째는 import만, body 재실행 없음.

### Cycle detection
이미 namespace 결정 (project_shell_namespace.md) — lazy load + cycle detect error.

## `source` vs `use namespace` 의미 분리

| 메커니즘 | 동작 | namespace 자동 처리 |
|---|---|---|
| `source PATH` (= `. PATH`) | POSIX 그대로 — file 내용을 caller scope에서 실행 | × |
| `use namespace .foo.bar` | Module resolution + load + import | ✓ |

`source`: generic include, raw, ksh93 정합.
`use namespace`: module system, path 자동 매핑.

## Manifest — frontend 책임

kash core는 *manifest 없음*. Module folder의 metadata 파일 (`MANIFEST`, `kash-module.toml` 등) 은 *third-party frontend* 책임.

### Slackware-style fully manual

- 사용자가 직접 modules/ 에 파일 복사 (tarball 풀기, git clone, cp 등)
- kash core는 search + load만
- 의존성/버전 관리 — *kash 외부 frontend*

frontend 옵션:
- 시스템 package manager (Slackpkg 등)
- kash-specific frontend (예: `kashpkg`, 별도 third-party)
- git clone + Makefile + manual install
- 단순 `cp -r` + `chmod`

### kash의 책임 boundary

- **kash core**: file path → namespace 매핑, search path 검색, file load, registry 관리
- **kash 외부**: distribution, versioning, dependency resolution, install, update, signing, security audit

분리 의도: kash 자체 minimal 유지 + frontend 다양성 보장.

## 예시

```
~/.local/share/kash/modules/
├── git-helpers.kash          # .git-helpers (init 함수, alias 등)
├── git-helpers/
│   ├── log.kash               # .git-helpers.log
│   └── diff.kash              # .git-helpers.diff
└── deploy/
    ├── staging.kash           # .deploy.staging
    └── prod.kash              # .deploy.prod
```

사용:
```sh
use namespace .git-helpers              # 전체 import
.git-helpers.log.format $hash           # 직접 접근

use namespace .deploy.staging           # specific submodule
.deploy.staging.run
```

수동 설치:
```sh
tar xzf git-helpers-1.0.tar.gz -C ~/.local/share/kash/modules/
# 또는
git clone URL ~/.local/share/kash/modules/git-helpers/
# 또는
cp myhelper.kash ~/.local/share/kash/modules/
```

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `use namespace` (resolution + load) | × | ✓ | × (ksh93에 없음) | ✓ | ✓ |
| File path → namespace 자동 매핑 | × | ✓ | × | ✓ | ✓ |
| `KASH_MODULE_PATH` 인식 | × | ✓ | × | ✓ | ✓ |

## 미결 (의도된 — frontend 영역)

- 자동 distribution / dependency 해소 — frontend 책임
- Manifest 표준 형식 — frontend별 자체 결정
- 모듈 versioning 메커니즘 — namespace path로 표현 (`.foo.v2.bar`), 별도 시스템 없음
- 충돌 검출 `warn-module-conflict` 옵션 추가 여부 — v1 또는 v2

**How to apply:** 향후 모듈 시스템 관련 결정 시 *kash core는 resolution만* 원칙 유지. distribution/management 관련 요청은 frontend 영역으로 redirect.
