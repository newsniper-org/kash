# Typeclasses

Scala 3.x의 `given/using` 모델을 기반으로 한 typeclass 시스템. Inheritance를 대체.

## 기본 구조

```sh
# Typeclass 선언 — 시그니처 + default impl
typeclass Showable {
    function show              # 시그니처만 (인스턴스가 반드시 제공)
}

typeclass Eq {
    function eq
}

typeclass Ord requires Eq {    # typeclass inheritance
    function lt
    function le { _.eq $1 || _.lt $1; }   # default impl, 다른 메소드 호출
    function gt { ! _.le $1; }
    function ge { ! _.lt $1; }
}

# Instance 선언
instance Showable for Int {
    function show { print "$_"; }
}

instance Ord for Int {
    function eq { (( _ == $1 )); }
    function lt { (( _ < $1 )); }
    # le, gt, ge는 default impl 자동 상속
}

# 사용 — dispatch는 runtime type 기반
x=5
echo $(x.show)
```

## Method dispatch resolution

`value.method` 호출 시:
1. 값 자신의 type 메소드 (`typeset -T` 정의된 것)
2. Lexical scope에서 보이는 typeclass instances
3. 못 찾으면 runtime error: "method `<name>` not found for type <Type>"

같은 메소드명이 여러 typeclass의 instance에 존재 시 → ambiguous, runtime error. 명시적 해소: `TypeclassName.method value args` form (예: `Eq.eq $a $b`).

## Coherence

- 같은 (Typeclass, Type) 페어의 instance가 여러 lexical scope에 존재 시: **innermost wins** (lexical).
- 같은 scope에 중복 정의: parse error.

## Scope rules

- `typeclass`, `instance` 선언은 함수처럼 lexically scoped.
- File top → 전체 file에서 가용.
- 함수 안 → 그 함수 내에서만 가용.
- Sourced file의 선언 → caller scope에 들어옴.

## Type inference + assertion (annotation 대체)

함수 정의 문법은 절대 건드리지 않음. Type annotation 대신:

- **Inference**: 런타임에 값의 type을 보고 자동 dispatch. 정적 분석 없음.
- **Assertion**: 함수 body에 명시적 제약 표현.

### 새 test 연산자 (`[[ ]]` 내부)

- `[[ x -is TypeName ]]` — type 정확 일치 (`${(t)x} == TypeName`)
- `[[ x -satisfies TypeclassName ]]` — x의 type이 해당 typeclass의 instance 보유

### `assert` builtin

```sh
assert <expression>
```
- expression이 false → error로 exit. error message는 expression source.
- default / `-secure`: 정상 동작.
- `-secure`에서는 비활성/우회 옵션 없음.

### 사용 패턴

```sh
function compare(a, b) {
    [[ $a -satisfies Ord ]] || die "a must satisfy Ord"
    [[ $b -satisfies Ord ]] || die "b must satisfy Ord"
    [[ ${(t)a} == ${(t)b} ]] || die "type mismatch"
    
    a.lt $b
}

# 또는 assert로 더 간결
function compare(a, b) {
    assert [[ $a -satisfies Ord ]]
    assert [[ $b -satisfies Ord ]]
    assert [[ ${(t)a} == ${(t)b} ]]
    
    a.lt $b
}
```

## Inheritance 폐기

typeset 확장의 "inheritance/composition" 항목 폐기. typeclass가 inheritance의 use case를 대체.

남은 OOP 확장 — 3종:
1. Constructor/destructor (dunder methods)
2. Private 멤버
3. Static / class 멤버

## v1 보류 (Scala 3에 있지만 미도입)

- Generic types in instances (`instance Showable for List[T] when T: Showable`)
- Type 파라미터 함수 (`function printAll[T: Showable]`)
- Multi-method dispatch
- Implicit conversion (`Conversion[A, B]`)
- Type-level introspection (`${(I)var}` 등)
- Function parameter type annotation (대체: type inference + assertion)

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `typeclass` 선언 | × | ✓ | × | ✓ | ✓ |
| `instance` 선언 | × | ✓ | × | ✓ | ✓ |
| Typeclass method dispatch | × | ✓ | × | ✓ | ✓ |
| `requires` (typeclass inheritance) | × | ✓ | × | ✓ | ✓ |
| `[[ -is ]]`, `[[ -satisfies ]]` | × | ✓ | × | ✓ | ✓ |
| `assert` builtin | × | ✓ | × | ✓ | ✓ |

## 미결

- `assert` 실패 시 정확한 동작 (exit vs return vs catchable exception)
- `-satisfies`의 transitive 처리 (Ord requires Eq면 `[[ x -satisfies Eq ]]`도 Ord instance 있으면 true?)
- Typeclass method가 이미 type의 native method와 충돌 시 처리 (native 우선, 그러나 명시적 `Tc.m value args`로 override 가능)
- Default method 안에서 typeclass의 다른 default method 호출 시 dispatch 순서
- Sourced file에서의 typeclass/instance import 정책 (전부 import? 명시적 export 필요?)
- POSIX-strict/-aware/ksh93u-strict에서 `assert` builtin의 처리 (`:` no-op로 대체, 비활성, error?)
