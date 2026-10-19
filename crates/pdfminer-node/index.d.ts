/// <reference types="node" />

/**
 * @fuelfwd/pdfminer: pdfminer.six (20260107) ported to Rust, with the same output.
 *
 * In options, a missing key is the Python default and `null` is Python's `None`.
 */

/** A PDF: its bytes, or a file path (opened like Python's `open(path, "rb")`). */
export type PdfInput = Buffer | Uint8Array | ArrayBuffer | string

/** `pdfminer.layout.LAParams`, in camelCase. */
export interface LAParamsOptions {
  /** default 0.5 */
  lineOverlap?: number
  /** default 2.0 */
  charMargin?: number
  /** default 0.5 */
  lineMargin?: number
  /** default 0.1 */
  wordMargin?: number
  /** default 0.5; `null` disables advanced layout analysis */
  boxesFlow?: number | null
  /** default false */
  detectVertical?: boolean
  /** default false */
  allTexts?: boolean
}

interface CommonOptions {
  password?: string
  /** 0-based page numbers; `null` or missing for every page */
  pageNumbers?: number[] | null
  /** 0 for no limit */
  maxpages?: number
  caching?: boolean
}

/** `extract_text(...)` */
export interface ExtractTextOptions extends CommonOptions {
  codec?: string
  /** default `LAParams()`; `null` is `laparams=None` (replaced by `LAParams()`, as in Python) */
  laparams?: LAParamsOptions | null
}

/** `extract_pages(...)` */
export interface ExtractPagesOptions extends CommonOptions {
  /** default `LAParams()`; `null` is `laparams=None` (replaced by `LAParams()`, as in Python) */
  laparams?: LAParamsOptions | null
}

/** `extract_text_to_fp(...)` into a buffer. */
export interface ConvertOptions extends CommonOptions {
  /** 'text' (default), 'html', 'xml', 'hocr' or 'tag' */
  outputType?: string
  /** default 'utf-8'; `null` writes `str` (HTML/XML/hOCR), which the buffer cannot take */
  codec?: string | null
  /** default `null` (no layout analysis), as in Python */
  laparams?: LAParamsOptions | null
  scale?: number
  /** degrees added to each page's rotation */
  rotation?: number
  /** 'normal' (default), 'exact' or 'loose' (HTML) */
  layoutmode?: string
  /** a directory to write the page images to */
  outputDir?: string | null
  stripControl?: boolean
}

/** The canonical layout dump (`pdfminer-rs dump`). */
export interface DumpLayoutOptions {
  password?: string
  /** default `LAParams()`; `null` runs no layout analysis */
  laparams?: LAParamsOptions | null
}

// --- Layout objects, in the format of tools/oracle.py --------------------------------------
// Numbers are floats (Python ints and floats alike, NaN and Infinity included); values pdfminer
// keeps untyped are encoded: bytes -> {bytes: hex}, a PDF name -> {lit: name}, a keyword ->
// {kwd: hex}, a dict -> {dict: [[key, value], ...]}, a reference -> {ref: objid}, a stream ->
// {stream: objid}.

export type Bbox = [number, number, number, number]
export type Matrix = [number, number, number, number, number, number]
export type Point = [number, number]
export type PyValue =
  | null
  | boolean
  | number
  | string
  | PyValue[]
  | { bytes: string }
  | { lit: string | { bytes: string } }
  | { kwd: string }
  | { dict: [string, PyValue][] }
  | { ref: number }
  | { stream: number | null }

/** A gray level, (r, g, b), (c, m, y, k), a colored pattern, or an uncolored pattern. */
export type Color = number | number[] | { pattern: string } | { base: number | number[]; pattern: string }

export interface GraphicState {
  linewidth: number
  linecap: PyValue
  linejoin: PyValue
  miterlimit: PyValue
  dash: [PyValue, PyValue] | null
  intent: PyValue
  flatness: PyValue
  scolor: Color
  scs: string
  ncolor: Color
  ncs: string
}

export interface LTChar {
  type: 'LTChar'
  bbox: Bbox
  text: string
  fontname: PyValue
  size: number
  adv: number
  upright: boolean
  matrix: Matrix
  ncs: string
  /** The graphic state at the end of the character's `q`/`Q` block, as pdfminer.six keeps it. */
  graphicstate: GraphicState
}

export interface LTAnno {
  type: 'LTAnno'
  text: string
}

export interface LTCurve {
  type: 'LTCurve' | 'LTLine' | 'LTRect'
  bbox: Bbox
  pts: Point[]
  linewidth: number
  stroke: boolean
  fill: boolean
  evenodd: boolean
  stroking_color: Color | null
  non_stroking_color: Color | null
  /** (operator, ...points) segments, points already transformed */
  original_path: [string, ...Point[]][] | null
  dashing_style: [PyValue, PyValue] | null
}

export interface LTImage {
  type: 'LTImage'
  bbox: Bbox
  name: string
  srcsize: [PyValue, PyValue]
  imagemask: PyValue
  bits: PyValue
  colorspace: PyValue
  stream: { stream: number | null }
}

export interface LTTextLine {
  type: 'LTTextLineHorizontal' | 'LTTextLineVertical'
  bbox: Bbox
  items: Array<LTChar | LTAnno>
}

export interface LTTextBox {
  type: 'LTTextBoxHorizontal' | 'LTTextBoxVertical'
  bbox: Bbox
  index: number
  items: LTTextLine[]
}

export interface LTFigure {
  type: 'LTFigure'
  bbox: Bbox
  name: string
  matrix: Matrix
  items: LTItem[]
}

/** `LTPage.groups`: text groups, referring to the page's text boxes by index. */
export type TextGroup =
  | { type: 'LTTextGroupLRTB' | 'LTTextGroupTBRL'; bbox: Bbox; items: TextGroup[] }
  | { type: 'LTTextBoxHorizontal' | 'LTTextBoxVertical'; index: number }

export type LTItem = LTChar | LTAnno | LTCurve | LTImage | LTTextLine | LTTextBox | LTFigure

export interface Page {
  type: 'LTPage'
  bbox: Bbox
  pageid: number
  rotate: number
  groups: TextGroup[] | null
  items: LTItem[]
}

/** A Python exception raised by pdfminer. */
export declare class PdfminerError extends Error {
  /** The exception class, e.g. `PDFSyntaxError`. */
  pythonClass: string
  /** `str(e)` */
  pythonMessage: string
}

/** `pdfminer.high_level.extract_text`: the text of the PDF. */
export declare function extractText(pdf: PdfInput, options?: ExtractTextOptions): string
export declare function extractTextAsync(pdf: PdfInput, options?: ExtractTextOptions): Promise<string>

/** `pdfminer.high_level.extract_pages`: the analysed layout of every page. */
export declare function extractPages(pdf: PdfInput, options?: ExtractPagesOptions): Page[]
export declare function extractPagesAsync(pdf: PdfInput, options?: ExtractPagesOptions): Promise<Page[]>

/** `pdfminer.high_level.extract_text_to_fp` into a buffer: text, HTML, XML, hOCR or tags. */
export declare function convert(pdf: PdfInput, options?: ConvertOptions): Buffer
export declare function convertAsync(pdf: PdfInput, options?: ConvertOptions): Promise<Buffer>

/**
 * The canonical layout dump: one JSON line per page (the format of `extractPages`), then a status
 * line, `{"status":"ok"}` or `{"status":"error","exc":Class,"msg":str(e)}`. Byte for byte what
 * `tools/oracle.py layout` prints with pdfminer.six.
 */
export declare function dumpLayout(pdf: PdfInput, options?: DumpLayoutOptions): string
export declare function dumpLayoutAsync(pdf: PdfInput, options?: DumpLayoutOptions): Promise<string>

/** The pdfminer.six release the port follows, e.g. "20260107". */
export declare const upstreamVersion: string
