; Python 抽取规则（第二门语言适配器——验证「加语言 = grammar + queries + 映射」的插件化承诺）。
; 分组结构与 typescript/queries.scm 完全一致，parse.rs 按语言选择对应规则集；
; 缺少的组（python 没有 jsx）编译时自然跳过。
; import/局部名/类基类走 parse.rs 手写遍历（python 子句形态与 TS 完全不同，query 不划算）。

; --- group: defs ---
; 顶层 def 是模块导出（python 语义：一切可导入），方法的导出性与归属在 parse.rs 里按祖先判定。
(function_definition name: (identifier) @def.name) @def.node
(class_definition name: (identifier) @def.name) @def.node

; --- group: calls ---
; 裸名调用与属性调用；构造调用在 python 里就是裸名调用类名，无需 new_expression。
(call function: (identifier) @call.name) @call.node
(call function: (attribute object: (_) @call.recv attribute: (identifier) @call.prop)) @call.node
