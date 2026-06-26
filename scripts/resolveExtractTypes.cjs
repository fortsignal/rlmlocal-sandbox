#!/usr/bin/env node
/**
 * resolveExtractTypes.cjs — the executor's TypeScript-compiler service. ONE script, TWO read-only ops (dispatch on
 * the stdin `op` field) so the executor has a single TS entry point (and one tsconfig load), not parallel scripts:
 *
 *   (default, op absent)  TYPE RESOLUTION — ground-truth param types for deterministic extraction. The extractor
 *     needs each helper-param's TYPE for a strict-TS signature; inferring types in the AST is reimplementing tsc →
 *     endless `any`. So we ASK the compiler. In: { root, file, positions:[{line,character}] } (0-based tree-sitter
 *     coords). Out: { types:[string|null] } — null where unresolvable. The brain keeps its AST scope/output analysis.
 *
 *   op:'refactor'  REFACTOR EDITS — move / extract / rename via the TS LANGUAGE SERVICE (getApplicableRefactors /
 *     getEditsForRefactor / findRenameLocations) — the reference-aware mechanics our hand-rolled mover refuses. In:
 *     { root, file, op:'refactor', kind:'move'|'extract'|'rename', args }. Out: { ok, refactor, edits:[{file,search,
 *     replace}] } — the SAME Edit shape the executor's apply_edits/verify_patch/apply_patch already apply (offsets are
 *     converted to search/replace hunks here, so the whole apply/verify/land pipeline is reused UNCHANGED).
 *
 * Runs on the EXECUTOR (it has the project + tsconfig + node_modules/@types). Read-only — never writes the project.
 */
const ts = require('typescript');
const path = require('node:path');
const fs = require('node:fs');

/** Load the project via tsconfig so the compiler sees every cross-file reference. Shared by both ops. */
function loadProject(root) {
  let fileNames = [];
  let options = { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext, strict: false, allowJs: true };
  const configPath = ts.findConfigFile(root, ts.sys.fileExists, 'tsconfig.json');
  if (configPath) {
    const { config, error } = ts.readConfigFile(configPath, ts.sys.readFile);
    if (!error && config) {
      const parsed = ts.parseJsonConfigFileContent(config, ts.sys, path.dirname(configPath));
      if (parsed.fileNames.length) fileNames = parsed.fileNames;
      if (parsed.options) options = parsed.options;
    }
  }
  return { configPath, fileNames, options };
}

// ───────────────────────── op:'refactor' — compute reference-aware EDITS via the language service ─────────────────────────

/** ADAPTER → the EXISTING executor edit shape. apply_edits (lib.rs) applies SEARCH/REPLACE hunks, not offsets — and
 *  the brain's own plan already uses that shape. So convert the language service's offset `textChanges` into it: slice
 *  each file at the offsets, EXPAND to whole lines (readable hunks = the human-approval invariant), merge overlapping
 *  windows. New files → empty search (apply_edits treats empty-search-on-empty-file as CREATE). `file` = relative to root. */
function toHunks(root, fileEdits, overlay) {
  const cache = new Map();
  // `overlay` (converge only): serve the caller's VIRTUAL content for a file instead of disk — converge accumulates
  // unlanded edits, so its positions/hunks must anchor on the accumulated content, not the stale on-disk file.
  const readF = (abs) => { if (overlay && overlay[abs] != null) return overlay[abs]; if (!cache.has(abs)) cache.set(abs, fs.existsSync(abs) ? fs.readFileSync(abs, 'utf8') : ''); return cache.get(abs); };
  const hunks = [];
  for (const fe of fileEdits) {
    const rel = path.relative(root, fe.fileName);
    if (fe.isNewFile) { hunks.push({ file: rel, search: '', replace: fe.textChanges.map((c) => c.newText).join('') }); continue; }
    const text = readF(fe.fileName);
    const lineStart = (i) => { while (i > 0 && text[i - 1] !== '\n') i--; return i; };
    const lineEnd = (i) => { while (i < text.length && text[i] !== '\n') i++; return i; };
    const windows = [];
    let win = null;
    for (const c of [...fe.textChanges].sort((a, b) => a.start - b.start)) {
      const ls = lineStart(c.start), le = lineEnd(c.start + c.length);
      if (win && ls <= win.le) { win.le = Math.max(win.le, le); win.changes.push(c); }
      else { win = { ls, le, changes: [c] }; windows.push(win); }
    }
    for (const w of windows) {
      // GUARD: a pure insertion on a BLANK/empty line (e.g. an import added at main.ts's blank first line) gives an
      // EMPTY search — which apply_edits only accepts for NEW files. Extend the window forward to the next non-blank
      // content so the hunk has a real, locatable anchor (and never collides with the new-file CREATE semantics).
      let le = w.le;
      while (le < text.length && text.slice(w.ls, le).trim() === '') le = lineEnd(le + 1);
      const search = text.slice(w.ls, le);
      let replace = search;
      for (const c of [...w.changes].sort((a, b) => b.start - a.start)) { const rs = c.start - w.ls; replace = replace.slice(0, rs) + c.newText + replace.slice(rs + c.length); }
      hunks.push({ file: rel, search, replace });
    }
  }
  return hunks;
}

function computeRefactor(req) {
  const { root: rawRoot, file, kind, args = {}, content } = req; // `content` = converge's VIRTUAL content for `file` (overlay)
  if (!rawRoot || !file || !kind) return { ok: false, reason: 'refactor: need { root, file, kind, args }' };
  const root = path.resolve(rawRoot); // ABSOLUTE — must match tsconfig fileNames, or the LS treats relative-vs-absolute as DIFFERENT files → cross-file refs lost
  const absFile = path.resolve(root, file);
  if (content == null && !fs.existsSync(absFile)) return { ok: false, reason: `file not found: ${absFile}` };
  const overlay = content != null ? { [absFile]: content } : null; // serve virtual content for absFile (converge); null = read disk

  const { fileNames, options } = loadProject(root);
  if (!fileNames.includes(absFile)) fileNames.push(absFile);
  const host = {
    getScriptFileNames: () => fileNames,
    getScriptVersion: () => '1',
    getScriptSnapshot: (f) => (f === absFile && content != null) ? ts.ScriptSnapshot.fromString(content) : (fs.existsSync(f) ? ts.ScriptSnapshot.fromString(fs.readFileSync(f, 'utf8')) : undefined),
    getCurrentDirectory: () => root,
    getCompilationSettings: () => options,
    getDefaultLibFileName: (o) => ts.getDefaultLibFilePath(o),
    fileExists: ts.sys.fileExists,
    readFile: ts.sys.readFile,
    readDirectory: ts.sys.readDirectory,
    directoryExists: ts.sys.directoryExists,
    getDirectories: ts.sys.getDirectories,
  };
  const ls = ts.createLanguageService(host, ts.createDocumentRegistry());
  const prefs = { allowTextChangesInNewFiles: true };

  const src = content != null ? content : fs.readFileSync(absFile, 'utf8');
  const lineStarts = [0];
  for (let i = 0; i < src.length; i++) if (src[i] === '\n') lineStarts.push(i + 1);
  const offsetOf = (line) => lineStarts[Math.max(0, Math.min(line - 1, lineStarts.length - 1))];

  // RENAME — findRenameLocations: rename an EXISTING symbol everywhere (def + every ref, all files). The brain
  // computes the new name (GEPA/refineName) and passes it; this applies it correctly.
  if (kind === 'rename') {
    const ln = parseInt(args.line, 10) || 1;
    const oldName = args.oldName, newName = args.newName;
    if (!oldName || !newName) return { ok: false, reason: 'rename: need args { line, oldName, newName }' };
    const lineStart = offsetOf(ln);
    const col = src.slice(lineStart, offsetOf(ln + 1)).indexOf(oldName);
    if (col < 0) return { ok: false, reason: `'${oldName}' not found on line ${ln}` };
    let locs;
    try { locs = ls.findRenameLocations(absFile, lineStart + col, false, false, prefs); }
    catch (e) { return { ok: false, reason: `findRenameLocations threw: ${e.message}` }; }
    if (!locs || !locs.length) return { ok: false, reason: `'${oldName}' is not renameable there` };
    const byFile = new Map();
    for (const l of locs) { if (!byFile.has(l.fileName)) byFile.set(l.fileName, []); byFile.get(l.fileName).push({ start: l.textSpan.start, length: l.textSpan.length, newText: newName }); }
    return { ok: true, refactor: 'rename', from: oldName, to: newName, files: byFile.size, sites: locs.length,
             edits: toHunks(root, [...byFile].map(([fileName, textChanges]) => ({ fileName, isNewFile: false, textChanges })), overlay) };
  }

  // REFERENCES — the GROUND-TRUTH reference set for a symbol (def + every use, scope-exact, cross-file) via the
  // language service. The decouple "unsafe-by-hand" part: the BRAIN can't reliably find all refs of shared state
  // (shadowing, cross-file); this does. The brain then composes the `_ctx` (decl→field) + rewrites refs→`_ctx.x`.
  // Returns positions per file (rel to root) — generic compiler query, no edit assembly, no brain logic.
  if (kind === 'references') {
    const ln = parseInt(args.line, 10) || 1;
    const name = args.name;
    if (!name) return { ok: false, reason: 'references: need args { line, name }' };
    const lineStart = offsetOf(ln);
    const col = src.slice(lineStart, offsetOf(ln + 1)).indexOf(name);
    if (col < 0) return { ok: false, reason: `'${name}' not found on line ${ln}` };
    let locs;
    try { locs = ls.findRenameLocations(absFile, lineStart + col, false, false, prefs); }
    catch (e) { return { ok: false, reason: `findRenameLocations threw: ${e.message}` }; }
    if (!locs || !locs.length) return { ok: false, reason: `'${name}' has no references here` };
    const byFile = new Map();
    for (const l of locs) {
      const rel = path.relative(root, l.fileName);
      if (!byFile.has(rel)) byFile.set(rel, []);
      byFile.get(rel).push({ start: l.textSpan.start, length: l.textSpan.length });
    }
    return { ok: true, refactor: 'references', name, files: byFile.size, sites: locs.length,
             refs: [...byFile].map(([file, positions]) => ({ file, positions: positions.sort((a, b) => a.start - b.start) })) };
  }

  // ENCAPSULATE (decouple) — bundle shared module vars into a `ctxName` object: the decl(s) → one `export const _ctx
  // = {…}`, EVERY reference (cross-file, via findRenameLocations) → `_ctx.<name>`, + `import { _ctx }` injected into
  // other files. The BRAIN decides the vars + ctx name (classifyDecouple/GEPA) and passes them; this does the
  // reference-aware mechanics the frontend can't (scope-exact, cross-file). Generic transform ("encapsulate field").
  if (kind === 'encapsulate') {
    const ctxName = args.ctxName || '_ctx';
    const vars = Array.isArray(args.vars) ? args.vars : [];
    if (!vars.length) return { ok: false, reason: 'encapsulate: need args { ctxName, vars:[{name,line}] }' };
    const prog = ls.getProgram();
    const sf = prog && prog.getSourceFile(absFile);
    if (!sf) return { ok: false, reason: 'encapsulate: source file not loaded' };
    // MERGE-INTO-EXISTING: a prior decouple may already have created a module-level `_ctx` — accumulate the new fields
    // into THAT one bag (mirrors buildCtxHoistPlan's merge) rather than redeclaring it. Found here, applied below.
    const existingCtxStmt = sf.statements.find((s) => ts.isVariableStatement(s) && s.declarationList.declarations.some((d) => ts.isIdentifier(d.name) && d.name.text === ctxName)) || null;

    const fields = [];                 // { name, init, type }
    const declRemovals = [];           // { start, length } home-file statement spans
    const varNames = new Set();        // the hoisted var names (for import-specifier detection)
    const allLocs = [];                // { fileName, start, length, name } — every ref (excl. the home decl name)
    const fileEdits = new Map();
    const push = (f, tc) => { if (!fileEdits.has(f)) fileEdits.set(f, []); fileEdits.get(f).push(tc); };
    let firstDeclStart = Infinity;

    for (const v of vars) {
      const name = v && v.name;
      if (!name) return { ok: false, reason: 'encapsulate: each var needs a name' };
      varNames.add(name);
      let decl = null;
      for (const s of sf.statements) {
        if (!ts.isVariableStatement(s)) continue;
        for (const d of s.declarationList.declarations) if (ts.isIdentifier(d.name) && d.name.text === name) decl = { d, stmt: s };
      }
      if (!decl) return { ok: false, reason: `no top-level declaration for \`${name}\` (needs a simple module-level decl)` };
      if (decl.stmt.declarationList.declarations.length > 1) return { ok: false, reason: `\`${name}\` shares a declaration with other vars — can't split cleanly` };
      fields.push({ name, init: decl.d.initializer ? decl.d.initializer.getText(sf) : 'undefined', type: decl.d.type ? decl.d.type.getText(sf) : null });
      const stmtStart = decl.stmt.getStart(sf), stmtLen = decl.stmt.getEnd() - stmtStart;
      declRemovals.push({ start: stmtStart, length: stmtLen, name });
      if (stmtStart < firstDeclStart) firstDeclStart = stmtStart;

      const namePos = decl.d.name.getStart(sf);
      let locs;
      try { locs = ls.findRenameLocations(absFile, namePos, false, false, prefs); }
      catch (e) { return { ok: false, reason: `findRenameLocations threw for ${name}: ${e.message}` }; }
      for (const l of (locs || [])) {
        if (l.fileName === absFile && l.textSpan.start === namePos) continue; // the decl name itself → handled by the _ctx decl
        allLocs.push({ fileName: l.fileName, start: l.textSpan.start, length: l.textSpan.length, name });
      }
    }

    const allTyped = fields.every((f) => f.type);
    const newVals = fields.map((f) => `${f.name}: ${f.init}`).join(', ');
    const newTypes = fields.map((f) => `${f.name}: ${f.type}`).join('; ');
    if (existingCtxStmt) {
      // MERGE: append the new fields to the existing `_ctx` (preserve its `export` + type), and remove ALL the new
      // var decls (none becomes a fresh `_ctx`). One bag accumulates across successive decouples.
      const d = existingCtxStmt.declarationList.declarations.find((x) => ts.isIdentifier(x.name) && x.name.text === ctxName);
      const init = d.initializer;
      if (!init || !ts.isObjectLiteralExpression(init)) return { ok: false, reason: `existing \`${ctxName}\` isn't an object literal — can't merge` };
      if (d.type && !allTyped) return { ok: false, reason: `existing \`${ctxName}\` is typed but the new field types couldn't all be resolved` };
      const mergedInit = init.properties.length ? init.getText(sf).replace(/\s*\}\s*$/, `, ${newVals} }`) : `{ ${newVals} }`;
      const typeText = d.type ? `: ${d.type.getText(sf).replace(/\s*\}\s*$/, `; ${newTypes} }`)}` : '';
      const exp = existingCtxStmt.modifiers?.some((mm) => mm.kind === ts.SyntaxKind.ExportKeyword) ? 'export ' : '';
      const start = existingCtxStmt.getStart(sf), len = existingCtxStmt.getEnd() - start;
      push(absFile, { start, length: len, newText: `${exp}const ${ctxName}${typeText} = ${mergedInit};` });
      for (const r of declRemovals) push(absFile, { start: r.start, length: r.length, newText: '' });
    } else {
      // FRESH: the FIRST var's decl becomes the `_ctx` decl; the rest are removed.
      const ctxDecl = `export const ${ctxName}${allTyped ? `: { ${newTypes} }` : ''} = { ${newVals} };`;
      for (const r of declRemovals) push(absFile, { start: r.start, length: r.length, newText: r.start === firstDeclStart ? ctxDecl : '' });
    }

    // CROSS-FILE: an `import { x } from './home'` SPECIFIER is a rename-loc too — it must become `import { _ctx }`
    // (NOT `_ctx.x`). Per other file: split locs into import-specifiers (→ rewrite the import once to add `_ctx` +
    // drop hoisted names) vs VALUE refs (→ `_ctx.x`).
    const byFile = new Map();
    for (const l of allLocs) { if (!byFile.has(l.fileName)) byFile.set(l.fileName, []); byFile.get(l.fileName).push(l); }
    for (const [f, locs] of byFile) {
      if (f === absFile) { for (const l of locs) push(f, { start: l.start, length: l.length, newText: `${ctxName}.${l.name}` }); continue; }
      const osf = prog.getSourceFile(f);
      const specStarts = new Set();      // positions that are import specifiers (not value refs)
      const importRewrites = [];         // { start, length, keep:[] }
      let ctxAlreadyImported = false;
      if (osf) for (const s of osf.statements) {
        if (!ts.isImportDeclaration(s) || !s.importClause || !s.importClause.namedBindings || !ts.isNamedImports(s.importClause.namedBindings)) continue;
        const nb = s.importClause.namedBindings, elems = nb.elements;
        if (elems.some((e) => e.name.text === ctxName)) ctxAlreadyImported = true;
        const hoistedHere = elems.filter((e) => varNames.has(e.name.text));
        if (hoistedHere.length) {
          for (const e of hoistedHere) specStarts.add(e.name.getStart(osf));
          importRewrites.push({ start: nb.getStart(osf), length: nb.getEnd() - nb.getStart(osf), keep: elems.filter((e) => !varNames.has(e.name.text)).map((e) => e.getText(osf)) });
        }
      }
      for (const l of locs) if (!specStarts.has(l.start)) push(f, { start: l.start, length: l.length, newText: `${ctxName}.${l.name}` }); // value refs
      let added = ctxAlreadyImported;    // add `_ctx` to exactly ONE import in this file
      for (const ir of importRewrites) {
        const names = [...ir.keep]; if (!added) { names.push(ctxName); added = true; }
        push(f, { start: ir.start, length: ir.length, newText: `{ ${names.join(', ')} }` });
      }
    }

    const fileEditsArr = [...fileEdits].map(([fileName, textChanges]) => ({ fileName, isNewFile: false, textChanges }));
    return { ok: true, refactor: 'encapsulate', ctx: ctxName, vars: fields.map((f) => f.name),
             files: fileEditsArr.length, sites: fileEditsArr.reduce((n, fe) => n + fe.textChanges.length, 0),
             edits: toHunks(root, fileEditsArr, overlay) };
  }

  let range, refactorMatch;
  if (kind === 'move') {
    const ln = parseInt(args.line, 10) || 1;
    const lineStart = offsetOf(ln);
    const lineEnd = offsetOf(ln + 1) - 1;
    let s = lineStart; while (s < lineEnd && /\s/.test(src[s])) s++; // skip indentation → land ON the declaration
    range = { pos: s, end: Math.max(s, lineEnd) };
    refactorMatch = (r) => /move/i.test(r.name);
  } else if (kind === 'extract') {
    const s = offsetOf(parseInt(args.startLine, 10) || 1);
    const e = args.endLine ? (offsetOf((parseInt(args.endLine, 10) || 1) + 1) - 1) : s;
    range = { pos: s, end: Math.max(s, e) };
    refactorMatch = (r) => /extract/i.test(r.name);
  } else return { ok: false, reason: `unknown kind: ${kind} (use move|extract|rename)` };

  let apps;
  try { apps = ls.getApplicableRefactors(absFile, range, prefs, undefined, undefined); }
  catch (e) { return { ok: false, reason: `getApplicableRefactors threw: ${e.message}` }; }
  const r = apps.find(refactorMatch);
  if (!r) return { ok: false, reason: `no ${kind} refactor offered here (offered: ${apps.map((x) => x.name).join(', ') || 'none'})` };
  // EXTRACT: prefer the MODULE-SCOPE action ("Extract to function in module scope") — a real sibling helper whose
  // captured locals become explicit PARAMS — over the inner-function action (nested, closure-captured). MOVE: the
  // "Move to a new file" action. Else first offered (verify is the backstop).
  const a = (kind === 'extract' && r.actions.find((x) => /module scope/i.test(x.description)))
    || (kind === 'move' && r.actions.find((x) => /new file/i.test(x.name + ' ' + x.description)))
    || r.actions[0];

  let res;
  try { res = ls.getEditsForRefactor(absFile, {}, range, r.name, a.name, prefs, undefined); }
  catch (e) { return { ok: false, reason: `getEditsForRefactor threw: ${e.message}` }; }
  if (!res || !res.edits) return { ok: false, reason: 'no edits returned' };

  return {
    ok: true, refactor: r.name, action: a.name,
    edits: toHunks(root, res.edits.map((fc) => ({
      fileName: fc.fileName, isNewFile: !!fc.isNewFile,
      textChanges: fc.textChanges.map((tc) => ({ start: tc.span.start, length: tc.span.length, newText: tc.newText })),
    })), overlay),
  };
}

// ───────────────────────── default op — TYPE RESOLUTION (unchanged) ─────────────────────────

function resolveTypes(req) {
  const { root, file, positions, content } = req; // `content` = converge's VIRTUAL content for `file` (overlay)
  const configPath = ts.findConfigFile(root, ts.sys.fileExists, 'tsconfig.json');
  if (!configPath) return { types: positions.map(() => null) };
  const cfg = ts.readConfigFile(configPath, ts.sys.readFile);
  const parsed = ts.parseJsonConfigFileContent(cfg.config, ts.sys, path.dirname(configPath));
  const absFile = path.resolve(root, file);
  let host; // converge: a custom host that serves the VIRTUAL content for `file` (else createProgram reads stale disk)
  if (content != null) {
    host = ts.createCompilerHost(parsed.options);
    const orig = host.getSourceFile.bind(host);
    host.getSourceFile = (fileName, lang, onErr, shouldCreate) =>
      path.resolve(fileName) === absFile ? ts.createSourceFile(fileName, content, lang, true) : orig(fileName, lang, onErr, shouldCreate);
  }
  const program = ts.createProgram(parsed.fileNames, parsed.options, host);
  const checker = program.getTypeChecker();
  const sf = program.getSourceFile(file) || program.getSourceFile(absFile);
  if (!sf) return { types: positions.map(() => null) };
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
  return { types };
}

function main() {
  const req = JSON.parse(fs.readFileSync(0, 'utf8'));
  const out = req.op === 'refactor' ? computeRefactor(req) : resolveTypes(req);
  process.stdout.write(JSON.stringify(out));
}
main();
