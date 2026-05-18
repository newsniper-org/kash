# OOP Extensions for `typeset -T`

## Dunder methods — lifecycle 2종만

| Dunder | 호출 시점 | 인자 | 의미 |
|---|---|---|---|
| `__init` | 인스턴스 생성 시 | 생성 form 따라 | 초기화/검증 |
| `__del` | unset 또는 scope 종료 시 | 없음 | 정리 |

**원칙**: dunder는 *type-inherent lifecycle hook*만. 모든 *capability* (행위, 변환, 연산자)는 typeclass로.

### Capability → typeclass 매핑

| 기능 | 대신 사용할 typeclass |
|---|---|
| string conversion | `Showable` (`function show`) |
| debug repr | `Showable` 또는 별도 `Reprable` |
| 해시 | `Hashable` |
| callable instance | `Callable` |
| iteration | `Iterable` |
| `==`/`<`/`>` 등 | `Eq` / `Ord` |

이유: capability는 외부에서 retroactively 추가 가능해야 함 (typeclass 본성). lifecycle은 type 정의 자체에 속함.

표준 typeclass library는 별도 결정 (`.kash.std.*` 또는 prelude).

## 인스턴스 생성 form 3가지

| Form | 동작 |
|---|---|
| `MyType x` | allocate → `__init` (no args) 호출 |
| `MyType x(arg1, arg2)` | allocate → `__init arg1 arg2` 호출 |
| `MyType x=(field=val)` | allocate → 직접 필드 할당, **`__init` bypass** |

세 번째 form은 raw construction (역직렬화, 복제 등) escape hatch.

## Private 멤버

```kash
typeset -T Stack=(
    private function _validate { ... }
    private typeset _cached_top
    
    function push {
        _._validate          # 인스턴스 메소드 안 — OK
    }
)

s._validate                  # ERROR
echo $s._cached_top          # ERROR
```

**Class-private** (Java 비슷):
- 접근 허용: type 인스턴스 메소드, discipline, static method
- 접근 거부: 외부 코드
- 모듈/namespace-private 아님 — type 단위 boundary

## Static / class 멤버

```kash
typeset -T Counter=(
    typeset -i count=0
    static typeset -i total=0
    
    function __init {
        ((Counter.total++))
        _.count=${1:-0}
    }
    
    function __del {
        ((Counter.total--))
    }
    
    static function reset_all {
        Counter.total=0
    }
)
```

**Access**:
- `${TypeName.member}` — class-level 필드
- `TypeName.method args` — static method

**Static 메소드 안**:
- `_` 미정의 (인스턴스 없음)
- 잘못 참조 시 unset

**Type vs 변수 모호성**:
- 컨벤션: type=PascalCase, 변수=camel/snake (강제 X)
- 충돌 시 compound access 컨텍스트에서 type 우선
- 명시적 escape: `${.kash.types.Foo.bar}`

## 통합 예시

```kash
typeset -T Connection=(
    typeset host=""
    typeset -i port=0
    typeset -i fd=-1
    static typeset -A active
    
    function __init {
        _.host=$1; _.port=$2
        coproc _c tcp-connect "$_.host" "$_.port"
        _.fd=${_c[0]}
        Connection.active[$_.host:$_.port]=$_
    }
    
    function __del {
        _.close
        unset 'Connection.active[$_.host:$_.port]'
    }
    
    function send { printf '%s' "$1" >&$_.fd; }
    function recv { local line; read -r line <&$_.fd; print -- "$line"; }
    private function close { [[ $_.fd -ge 0 ]] && exec {_.fd}>&-; }
    
    static function close_all { ... }
)

# Capability는 typeclass로 (외부에서 추가)
instance Showable for Connection {
    function show {
        print "Connection($_.host:$_.port, fd=$_.fd)"
    }
}
```

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| Dunder (`__init`, `__del`) | × | ✓ | × | ✓ | ✓ |
| `MyType x(args)` constructor | × | ✓ | × | ✓ | ✓ |
| `private` | × | ✓ | × | ✓ | ✓ |
| `static` | × | ✓ | × | ✓ | ✓ |
| `TypeName.member` 접근 | × | ✓ | × | ✓ | ✓ |

## 미결

- `__del` 비정상 종료 시 보장 (best-effort)
- `__init` 실패 시 정책 (abort vs partial)
- `private` nameref 우회 처리
- `static`의 multi-process 의미 (각자 own copy 권장)
- 표준 typeclass library namespace 및 제공 시점
