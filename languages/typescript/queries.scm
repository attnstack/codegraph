; TypeScript/TSX 抽取规则（codegraph 语言的「适配器契约」）。
; 文件按 `; --- group: <name> ---` 分组，parse.rs 逐组独立编译：
; 某组因 grammar 版本差异编译失败时只降级该组并告警，不会让整个解析不可用。
; 加新语言 = 复制本文件改节点名 + 在 parse.rs 注册 Language，无需动其他模块。

; --- group: defs ---
; 声明抽取。name 字段用通配 (_)：TS grammar 把类名绑成 type_identifier、函数名绑成 identifier，
; 通配可以让同一份规则同时服务 TS 与 JS。
(function_declaration name: (_) @def.name) @def.node
(method_definition name: (property_identifier) @def.name) @def.node
(class_declaration name: (_) @def.name) @def.node
(abstract_class_declaration name: (_) @def.name) @def.node
(interface_declaration name: (_) @def.name) @def.node
(variable_declarator name: (identifier) @def.name value: (arrow_function)) @def.node
(variable_declarator name: (identifier) @def.name value: (function_expression)) @def.node
(public_field_definition name: (property_identifier) @def.name value: (arrow_function)) @def.node

; --- group: calls ---
; 调用点。裸标识符调用与成员调用分开捕获；new 表达式视作对类构造的调用。
(call_expression function: (identifier) @call.name) @call.node
(call_expression function: (member_expression object: (_) @call.recv property: (property_identifier) @call.prop)) @call.node
(new_expression constructor: (identifier) @new.name) @new.node
(new_expression constructor: (member_expression object: (_) @new.recv property: (property_identifier) @new.prop)) @new.node

; --- group: jsx ---
; JSX 组件标签视作调用：`<Button />` 与 Button(props) 在调用图上等价。
; 标签名在 Rust 侧从元素的命名子节点中提取（避免依赖 grammar 内部命名差异）。
(jsx_self_closing_element) @jsx.node
(jsx_opening_element) @jsx.node
