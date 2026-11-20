import io

p = 'Cargo.toml'
t = io.open(p, encoding='utf-8').read()
if 'version = "0.2.1"' in t:
    io.open(p, 'w', encoding='utf-8').write(t.replace('version = "0.2.1"', 'version = "0.2.2"'))
    print('版本 -> 0.2.2')

p = 'docs/34-versioning.md'
t = io.open(p, encoding='utf-8').read()
t = t.replace('''0.2.1   ← 网页端收尾 + 聊天流离线校验进门禁（不改变对外契约）
        224 个测试 / 51 项门禁 / 12 个攻击场景''',
'''0.2.1   ← 网页端收尾 + 聊天流离线校验进门禁（不改变对外契约）
0.2.2   ← **网页端脱敏闭合**（SSE + /snapshot + 工具全文库），隐私两档不再有缺口
        301 个测试 / 54 项门禁 / 13 个攻击场景''')
io.open(p, 'w', encoding='utf-8').write(t)
print('docs/34 已更新')

# README 的版本行
p = 'README.md'
t = io.open(p, encoding='utf-8').read()
t = t.replace('> **当前版本 0.2.1：可用，但不是生产就绪。**', '> **当前版本 0.2.2：可用，但不是生产就绪。**')
t = t.replace('- **0.2.1** · 测试与门禁由', '- **0.2.2** · 测试与门禁由')
io.open(p, 'w', encoding='utf-8').write(t)
print('README 已更新')
