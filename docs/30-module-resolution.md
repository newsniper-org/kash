# Module Resolution Convention

kash core는 *resolution + loading*만 책임. Slackware-style 완전 수동 관리.

## File path ↔ namespace (자동)

```
<MODULE_ROOT>/foo/bar.kash       ↔  .foo.bar
<MODULE_ROOT>/foo.kash            ↔  .foo
<MODULE_ROOT>/a/b/c/d.kash        ↔  .a.b.c.d
```

- 파일 내용 자동으로 `namespace .foo.bar { ... }` wrap
- 명시 가능 (일치해야 함, 불일치 시 error)

### Single-file vs directory

```
modules/
├── foo.kash              # .foo
└── foo/
    ├── bar.kash          # .foo.bar
    └── baz.kash          # .foo.baz
```

`foo.kash` + `foo/` 공존 OK. **No `__init__.kash`** — 단순화.

## Search paths (우선순위)

1. `KASH_MODULE_PATH` env var (`:` 구분)
2. `~/.local/share/kash/modules/` (user)
3. `/usr/local/share/kash/modules/` (system)
4. `/usr/share/kash/modules/` (built-in)

First-found-wins. 충돌 시 silent (또는 `warn-module-conflict` v1/v2 옵션 검토).

## `use namespace` 동작

```sh
use namespace .foo.bar
```

1. Registry 확인 — 이미 load되어 있으면 import만
2. Search path에서 `foo/bar.kash` 검색
3. 첫 발견 file load → `namespace .foo.bar { ... }` wrap
4. Registry 등록
5. Caller scope에 import

**Idempotent**: 두 번 호출 → 두 번째는 import만, body 재실행 없음.

## `source` vs `use namespace`

| 메커니즘 | 동작 | namespace |
|---|---|---|
| `source PATH` | POSIX raw include (caller scope) | × |
| `use namespace .foo.bar` | Module resolution + load + import | ✓ (자동) |

## Manifest — frontend 책임

kash core: *manifest 없음*. Metadata 파일은 third-party frontend 책임.

### Slackware-style 수동
- 사용자가 modules/에 파일 복사 (tarball, git clone, cp 등)
- kash는 search + load만
- 의존성/버전/install은 kash 외부

Frontend 옵션 (kash 외부): 시스템 package manager, kash-specific frontend (third-party `kashpkg` 등), git + Makefile, 단순 cp.

## 예시

```
~/.local/share/kash/modules/
├── git-helpers.kash          # .git-helpers
├── git-helpers/
│   ├── log.kash               # .git-helpers.log
│   └── diff.kash              # .git-helpers.diff
└── deploy/
    ├── staging.kash           # .deploy.staging
    └── prod.kash              # .deploy.prod
```

```sh
use namespace .git-helpers
.git-helpers.log.format $hash
use namespace .deploy.staging
.deploy.staging.run
```

수동 install:
```sh
tar xzf git-helpers-1.0.tar.gz -C ~/.local/share/kash/modules/
git clone URL ~/.local/share/kash/modules/git-helpers/
cp myhelper.kash ~/.local/share/kash/modules/
```

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `use namespace` | × | ✓ | × | ✓ | ✓ |
| Path → namespace 자동 매핑 | × | ✓ | × | ✓ | ✓ |
| `KASH_MODULE_PATH` | × | ✓ | × | ✓ | ✓ |

## 미결 (의도된 — frontend 영역)

- 자동 distribution / dependency 해소 → frontend
- Manifest 표준 형식 → frontend별
- 모듈 versioning → namespace path로 표현
- `warn-module-conflict` 옵션 → v1/v2 검토
