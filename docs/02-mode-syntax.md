# Mode Declaration Syntax

## 키워드 — `mode`

세 컨텍스트(file/function/block) 모두 `mode` 하나로 통일.
- ksh/bash와 충돌 없음
- zsh의 `emulate`보다 noun-기반 명료성
- 단일 키워드 = 학습 부담 최소

## 세 가지 form

```sh
mode <name>            # (1) unbounded — 현재 scope 끝까지 + 바깥으로도 전파
mode -L <name>         # (2) lexical — 현재 scope에 한정, exit 시 자동 복원
mode <name> { ... }    # (3) block — 즉시 새 블록, 자동 복원 (= { mode -L <name>; ...; })
```

### Form별 권장

- **File top**: `mode <name>` (자연스럽게 file 전체)
- **함수 안**: `mode -L <name>` 권장 (caller mode silent 변경 방지). `mode <name>` (unbounded)은 허용하지만 비-idiomatic.
- **블록**: `mode <name> { ... }` (가장 명확)

## Mode 이름 format

```
<base>[-<modifier>]*
```

- Base (소문자): `default`, `posix-strict`, `posix-aware`, `ksh93u-strict`
- Modifier (소문자): `-secure`, 향후 `-noglob`, `-noeval` 등
- 예: `default`, `default-secure`, `posix-aware-secure`, `default-secure-noglob`

## Modifier monotonicity

```sh
mode default-secure {
    mode posix-aware-secure { ... }    # OK — base 변경, -secure 유지
    mode default-secure-noglob { ... } # OK — modifier 추가
    mode default { ... }               # ✗ ERROR — -secure 제거 불가
}
```

Base mode는 자유롭게 변경 가능. Modifier는 monotonically non-decreasing.

## Shebang

```sh
#!/usr/bin/env <shellname> --mode=<name>
```

- `--mode=<name>` CLI flag로 file scope 초기 모드 설정
- 없으면 `default`
- 파일 내 `mode <name>` declaration이 있으면 shebang을 override

## Symlinks (CLI 인터페이스까지 호환)

전통적 쉘 호환을 위해 호출명에 따라 초기 mode 자동 설정 + **CLI 인수 인터페이스도 동일하게 모방**.

| 호출명 | 초기 mode | CLI 호환 대상 |
|---|---|---|
| (정규 이름) | `default` | 신규 쉘 자체 인터페이스 |
| `sh` | `posix-strict` | POSIX sh CLI 정확히 매칭 |
| `ksh` | `ksh93u-strict` | ksh93u+m CLI 정확히 매칭 |

**핵심 제약 — drop-in CLI 호환:**
- `sh`로 호출 시: POSIX sh의 flag set (`-c`, `-s`, `-i`, `-l`, `-` + set-option flags `-aCefhmnuvx` + `-o name` 등)을 *정확히 동일하게* 파싱하고 동작.
- `ksh`로 호출 시: ksh93u+m의 flag set 전체를 정확히 동일하게.
- 새 쉘 고유 flag (`--mode=` 등)는 symlink 호출 시 **거부** — 호환 약속을 깨지 않기 위해.

implementation 비용: 호환 대상별 CLI parser configuration 필요, flag 의미론 (예: `-h`의 의미가 POSIX/ksh/bash마다 다름)도 정확히 매칭.

## Runtime introspection

```sh
${.sh.mode}              # 현재 mode 문자열 (예: "default-secure")
${.sh.mode.base}         # "default", "posix-strict", ...
${.sh.mode.modifiers}    # array — ("secure" "noglob")
```

`.sh.mode`는 compound var — ksh93의 `.sh.*` reserved namespace 정합 + 분해 접근 자연스러움.

## 기본값

- Script (no shebang flag, no pragma, no symlink): `default`
- Interactive shell: `default`
- Sourced file: caller mode 상속

## 유효성

- 알 수 없는 mode 이름 → parse error
- 중복 modifier (`default-secure-secure`) → parse error
- Base 없이 modifier만 (`-secure`) → parse error
- modifier monotonicity 위반 → parse/runtime error

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `mode <name>` (unbounded) | × | ✓ | × | ✓ | ✓ |
| `mode -L <name>` | × | ✓ | × | ✓ | ✓ |
| `mode <name> { ... }` | × | ✓ | × | ✓ | ✓ |
| `${.sh.mode}` introspection | × | ✓ | ✓ | ✓ | ✓ |

POSIX-strict와 ksh93u-strict는 `mode` 키워드 자체 비활성 — strict 모드는 정의상 escape 불가.
