'use strict'

// Tests of @fuelfwd/pdfminer. The layout objects are checked against the canonical dump (which
// tools/difftest.py checks against pdfminer.six byte for byte); when `uv` and the Python
// reference are available, the dump and the converters are also compared with Python directly.

const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const pdfminer = require('..')

const ROOT = path.resolve(__dirname, '..', '..', '..')
const SAMPLES = path.join(ROOT, 'upstream', 'pdfminer.six', 'samples')
const CORPUS = path.join(ROOT, 'tests', 'corpus')

function pdfs(dir) {
  const out = []
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name)
    if (e.isDirectory()) out.push(...pdfs(p))
    else if (e.name.endsWith('.pdf')) out.push(p)
  }
  return out.sort()
}

const FILES = [...pdfs(SAMPLES), ...pdfs(CORPUS)]
// Encrypted samples open with these passwords (as in the upstream tests).
const PASSWORDS = {
  'aes-128.pdf': 'foo',
  'aes-128-m.pdf': 'foo',
  'aes-256.pdf': 'foo',
  'aes-256-m.pdf': 'foo',
  'aes-256-r6.pdf': 'usersecret',
  'rc4-40.pdf': 'foo',
  'rc4-128.pdf': 'foo',
}
// Python needs minutes for these; they are covered by tools/difftest.py.
const SLOW = new Set(['i1040nr.pdf', 'f1040nr.pdf', 'nlp2004slides.pdf', 'naacl06-shinyama.pdf', 'kampo.pdf'])

function password(file) {
  return PASSWORDS[path.basename(file)] ?? ''
}

// JSON.parse for the dump, which has NaN, Infinity and -Infinity like Python's json.dumps.
function parseDump(text) {
  let out = ''
  let inString = false
  for (let i = 0; i < text.length; i++) {
    const c = text[i]
    if (inString) {
      out += c
      if (c === '\\') out += text[++i]
      else if (c === '"') inString = false
    } else if (c === '"') {
      inString = true
      out += c
    } else if (text.startsWith('NaN', i)) {
      out += '"\\u0000NaN"'
      i += 2
    } else if (text.startsWith('-Infinity', i)) {
      out += '"\\u0000-Infinity"'
      i += 8
    } else if (text.startsWith('Infinity', i)) {
      out += '"\\u0000Infinity"'
      i += 7
    } else {
      out += c
    }
  }
  const special = { '\u0000NaN': NaN, '\u0000Infinity': Infinity, '\u0000-Infinity': -Infinity }
  return JSON.parse(out, (k, v) => (typeof v === 'string' && v in special ? special[v] : v))
}

function dumpLines(text) {
  const lines = text.trimEnd().split('\n').map(parseDump)
  return { pages: lines.slice(0, -1), status: lines[lines.length - 1] }
}

test('extractText gives the upstream test strings', () => {
  const s = (f) => path.join(SAMPLES, f)
  assert.equal(
    pdfminer.extractText(s('simple1.pdf')),
    'Hello \n\nWorld\n\nHello \n\nWorld\n\nH e l l o  \n\nW o r l d\n\nH e l l o  \n\nW o r l d\n\n\f',
  )
  assert.equal(pdfminer.extractText(s('simple2.pdf')), '\f')
  assert.equal(
    pdfminer.extractText(s('simple3.pdf')),
    'Hello\n\nHello\nあ\nい\nう\nえ\nお\nあ\nい\nう\nえ\nお\nWorld\n\nWorld\n\n\f',
  )
  assert.equal(pdfminer.extractText(fs.readFileSync(s('simple4.pdf'))), 'Text1\nText2\nText3\n\n\f')
  assert.equal(pdfminer.extractText(s('contrib/issue_566_test_2.pdf')).trim(), '甲方：中国饮料有限公司（盖章）')
  assert.equal(
    pdfminer.extractText(s('contrib/issue-791-non-unicode-cmap.pdf')).trim(),
    'Peněžní prostředky na účtech',
  )
  // laparams (camelCase LAParams fields; boxesFlow null disables advanced analysis)
  const boxes = (lp) =>
    pdfminer
      .extractPages(s('simple4.pdf'), { laparams: lp })[0]
      .items.filter((i) => i.type.startsWith('LTTextBox')).length
  assert.equal(boxes({ lineMargin: 0.19 }), 3)
  assert.equal(boxes({ lineMargin: 0.21 }), 1)
  assert.equal(boxes({ boxesFlow: null }), 1)
})

test('extractPages equals the canonical dump on every sample', () => {
  for (const file of FILES) {
    const options = { password: password(file) }
    const { pages, status } = dumpLines(pdfminer.dumpLayout(file, options))
    let got
    try {
      got = pdfminer.extractPages(file, options)
    } catch (err) {
      assert.ok(err instanceof pdfminer.PdfminerError, `${file}: ${err}`)
      assert.equal(status.status, 'error', file)
      assert.equal(err.pythonClass, status.exc, file)
      assert.equal(err.pythonMessage, status.msg, file)
      continue
    }
    assert.equal(status.status, 'ok', file)
    assert.deepEqual(got, pages, file)
  }
})

test('the async functions give what the sync ones give', async () => {
  const file = path.join(SAMPLES, 'nonfree', 'dmca.pdf')
  assert.equal(await pdfminer.extractTextAsync(file), pdfminer.extractText(file))
  assert.deepEqual(await pdfminer.extractPagesAsync(file), pdfminer.extractPages(file))
  assert.deepEqual(await pdfminer.convertAsync(file, { outputType: 'xml' }), pdfminer.convert(file, { outputType: 'xml' }))
  assert.equal(await pdfminer.dumpLayoutAsync(file, { laparams: null }), pdfminer.dumpLayout(file, { laparams: null }))
  await assert.rejects(pdfminer.extractTextAsync(Buffer.from('%PDF-1.4 nothing')), (err) => {
    assert.equal(err.pythonClass, 'PDFSyntaxError')
    return true
  })
})

test('Python exceptions become PdfminerError', () => {
  const cases = [
    [() => pdfminer.extractText(Buffer.from('not a pdf')), 'PDFSyntaxError', 'No /Root object! - Is this really a PDF?'],
    [
      () => pdfminer.extractText('/nonexistent/a.pdf'),
      'FileNotFoundError',
      "[Errno 2] No such file or directory: '/nonexistent/a.pdf'",
    ],
    [() => pdfminer.extractText(path.join(SAMPLES, 'encryption', 'aes-128.pdf'), { password: 'x' }), 'PDFPasswordIncorrect', ''],
    [
      () => pdfminer.convert(path.join(SAMPLES, 'simple1.pdf'), { outputType: 'bogus' }),
      'PDFValueError',
      'Output type can be text, html, xml or tag but is bogus',
    ],
    [
      () => pdfminer.extractText(path.join(SAMPLES, 'simple1.pdf'), { laparams: { boxesFlow: 2 } }),
      'PDFValueError',
      'LAParam boxes_flow should be None, or a number between -1 and +1',
    ],
  ]
  for (const [fn, cls, msg] of cases) {
    assert.throws(fn, (err) => {
      assert.ok(err instanceof pdfminer.PdfminerError)
      assert.equal(err.name, 'PdfminerError')
      assert.equal(err.pythonClass, cls)
      assert.equal(err.pythonMessage, msg)
      return true
    })
  }
  assert.throws(() => pdfminer.extractText(42), TypeError)
  assert.throws(() => pdfminer.extractText(path.join(SAMPLES, 'simple1.pdf'), { maxpages: 'x' }), /maxpages/)
})

test('page selection and inputs', () => {
  const file = path.join(SAMPLES, 'nonfree', 'dmca.pdf')
  const all = pdfminer.extractPages(file)
  assert.ok(all.length > 1)
  assert.deepEqual(pdfminer.extractPages(file, { maxpages: 1 }), all.slice(0, 1))
  // pageid counts the pages processed, as in pdfminer.six
  assert.deepEqual(pdfminer.extractPages(file, { pageNumbers: [1] }), [{ ...all[1], pageid: 1 }])
  const bytes = fs.readFileSync(file)
  assert.equal(pdfminer.extractText(new Uint8Array(bytes)), pdfminer.extractText(file))
  assert.equal(pdfminer.upstreamVersion, '20260107')
})

test('text keeps lone surrogates, like Python str', () => {
  const file = path.join(CORPUS, 'surrogates-identity.pdf')
  const text = pdfminer.extractText(file)
  const { pages } = dumpLines(pdfminer.dumpLayout(file))
  const chars = []
  const walk = (o) => {
    if (o.type === 'LTChar' || o.type === 'LTAnno') chars.push(o.text)
    for (const i of o.items ?? []) walk(i)
  }
  pages.forEach(walk)
  assert.ok(/[\uD800-\uDFFF]/.test(chars.join('')), 'the corpus file has lone surrogates')
  assert.ok(/[\uD800-\uDFFF]/.test(text))
})

// --- Against pdfminer.six itself (needs uv and the Python reference) ----------------------

const uv = spawnSync('uv', ['--version'], { cwd: ROOT })
const havePython = uv.status === 0

function python(args) {
  const p = spawnSync('uv', ['run', 'python', ...args], { cwd: ROOT, maxBuffer: 1 << 30, encoding: 'buffer' })
  return p.stdout
}

test('dumpLayout is pdfminer.six byte for byte', { skip: !havePython && 'uv is not available' }, () => {
  for (const file of FILES) {
    if (SLOW.has(path.basename(file))) continue
    for (const laparams of ['default', 'none']) {
      const py = python(['tools/oracle.py', 'layout', file, '--laparams', laparams, '--password', password(file)])
      const rs = pdfminer.dumpLayout(file, { password: password(file), laparams: laparams === 'none' ? null : undefined })
      assert.equal(rs, py.toString('utf8'), `${file} (${laparams})`)
    }
  }
})

test('convert is pdf2txt.py byte for byte', { skip: !havePython && 'uv is not available' }, () => {
  const types = [
    ['text', []],
    ['html', []],
    ['xml', []],
    ['hocr', []],
    ['tag', []],
  ]
  for (const file of FILES) {
    if (SLOW.has(path.basename(file)) || file.includes('issue-598-cmap-other-fonts')) continue
    for (const [outputType, extra] of types) {
      const py = python(['tools/pdf2txt_ref.py', '-t', outputType, ...extra, '-P', password(file), file])
      let rs
      try {
        rs = pdfminer.convert(file, { outputType, password: password(file), laparams: {} })
      } catch (err) {
        // Python printed what it wrote before the exception.
        assert.ok(err instanceof pdfminer.PdfminerError, `${file}: ${err}`)
        continue
      }
      assert.ok(rs.equals(py), `${file} (${outputType})`)
    }
  }
})
