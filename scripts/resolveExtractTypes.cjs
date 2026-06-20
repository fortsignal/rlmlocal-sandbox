#!/usr/bin/env node
/**
 * resolveExtractTypes.cjs — GROUND-TRUTH type resolution for deterministic extraction.
 *
 * The extractor needs each helper-param's TYPE for a strict-TS signature. Inferring types in the AST is
 * reimplementing tsc → endless `any` fallbacks. Instead we ASK the compiler. Runs on the EXECUTOR (it has the
 * project + tsconfig + node_modules/@types). Input (stdin JSON): { root, file, positions:[{line,character}] }
 * (0-based, tree-sitter coords). Output (stdout JSON): { types:[string|null] } — null where it can't resolve.
 * Pure query, read-only. The brain keeps its AST scope/output analysis; only the type STRINGS come from here.
 */
const ts = require('typescript');
const path = require('node:path');

function main() {
  const { root, file, positions } = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
  const configPath = ts.findConfigFile(root, ts.sys.fileExists, 'tsconfig.json');
  if (!configPath) { process.stdout.write(JSON.stringify({ types: positions.map(() => null) })); return; }
  const cfg = ts.readConfigFile(configPath, ts.sys.readFile);
  const parsed = ts.parseJsonConfigFileContent(cfg.config, ts.sys, path.dirname(configPath));
  const program = ts.createProgram(parsed.fileNames, parsed.options);
  const checker = program.getTypeChecker();
  const sf = program.getSourceFile(file);
  if (!sf) { process.stdout.write(JSON.stringify({ types: positions.map(() => null) })); return; }
  const idAt = (pos) => {
    let hit;
    (function find(node) {
      if (pos < node.getStart(sf) || pos >= node.getEnd()) return;
      if (ts.isIdentifier(node)) hit = node;
      node.forEachChild(find);
    })(sf);
    return hit;
  };
  // SCOPE GUARD: typeToString can name a type (e.g. `VFile`) that the ORIGINAL relied on by INFERENCE but isn't
  // imported at the insertion point → writing it explicitly = TS2552 "Cannot find name". Replace any type-name
  // NOT resolvable at the file scope with `any`, so the annotation always compiles: `{ file: VFile; score: number }[]`
  // → `{ file: any; score: number }[]` (keeps the structure → `.map` etc. still type, no TS7006). Built-ins kept.
  const BUILTIN = new Set(['Array','ReadonlyArray','Map','Set','WeakMap','WeakSet','Promise','Record','Partial','Readonly','Required','Pick','Omit','Exclude','Extract','NonNullable','ReturnType','Parameters','InstanceType','String','Number','Boolean','Object','Date','RegExp','Error','Function','Symbol','BigInt','Uint8Array','Int32Array','Float64Array','Iterable','AsyncIterable','Iterator','Generator','ArrayBuffer','JSON','Math']);
  const inScope = (nm) => {
    if (typeof checker.resolveName === 'function') {
      try { return !!checker.resolveName(nm, sf, ts.SymbolFlags.Type | ts.SymbolFlags.Value, false); } catch { /* fall through */ }
    }
    try { return new RegExp('\\b' + nm + '\\b').test(sf.getText()); } catch { return true; }
  };
  const sanitize = (s) => s.replace(/\b[A-Z][A-Za-z0-9_]*\b/g, (nm) => (BUILTIN.has(nm) || inScope(nm)) ? nm : 'any');
  const types = positions.map((p) => {
    try {
      const pos = ts.getPositionOfLineAndCharacter(sf, p.line, p.character);
      const id = idAt(pos);
      if (!id) return null;
      // WIDEN literal types for a PARAMETER: a `const CAP = 3` has the narrow literal type `3`, but a parameter
      // annotated `: 3` would only accept the literal 3 — defeating the point of parameterizing it (and the bad
      // annotation travels when the fn later moves). getBaseTypeOfLiteralType widens `3`→number, `'a'|'b'`→string,
      // `true`→boolean; it's a no-op on non-literals. The resolver's job IS param types, so widening belongs here.
      const t0 = checker.getTypeAtLocation(id);
      const t = typeof checker.getBaseTypeOfLiteralType === 'function' ? checker.getBaseTypeOfLiteralType(t0) : t0;
      const raw = checker.typeToString(t);
      if (!raw || raw === 'error') return null;
      let s = sanitize(raw);
      // EXPAND a NAMED object type the guard just nuked to bare `any` (e.g. `FileMetricsResult`) into its STRUCTURE
      // `{ top: …[]; target: …; median: number }`, sanitizing each member. A bare `any` param drops the shape, so
      // callbacks over it (`res.top.filter(f=>…)`) become implicit-any → TS7006 RED; keeping the object/array shape
      // makes them contextually typed. Arrays (`X[]`→`any[]`) + functions never hit this (raw isn't bare `any`).
      if (s === 'any' && (t.getFlags() & ts.TypeFlags.Object) && !(t.getCallSignatures && t.getCallSignatures().length)) {
        try {
          const props = t.getProperties();
          if (props.length && props.length <= 40) {
            const members = props.map((p) => {
              const pt = checker.getTypeOfSymbolAtLocation(p, id);
              return `${p.getName()}: ${sanitize(checker.typeToString(pt)) || 'any'}`;
            });
            s = `{ ${members.join('; ')} }`;
          }
        } catch { /* keep `any` → null → AST fallback */ }
      }
      return s && s !== 'any' ? s : null; // bare `any` (or all-unresolvable) → null → AST fallback
    } catch { return null; }
  });
  process.stdout.write(JSON.stringify({ types }));
}
main();
