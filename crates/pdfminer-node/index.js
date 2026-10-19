'use strict'

// @fuelfwd/pdfminer: pdfminer.six ported to Rust, with the same output.
//
// Every function takes the PDF as a Buffer (or Uint8Array) or a file path, and an options object
// in which a missing key is the Python default and `null` is Python's `None`. A Python exception
// is thrown as a PdfminerError whose `pythonClass` is the exception class (`PDFSyntaxError`) and
// whose `pythonMessage` is `str(e)`.

const native = require('./binding.js')

class PdfminerError extends Error {
  constructor(pythonClass, pythonMessage) {
    super(pythonMessage ? `${pythonClass}: ${pythonMessage}` : pythonClass)
    this.name = 'PdfminerError'
    this.pythonClass = pythonClass
    this.pythonMessage = pythonMessage
  }
}

// The native code reports a Python exception as "\0Class\0message".
function convertError(err) {
  if (err && typeof err.message === 'string' && err.message.startsWith('\u0000')) {
    const end = err.message.indexOf('\u0000', 1)
    return new PdfminerError(err.message.slice(1, end), err.message.slice(end + 1))
  }
  return err
}

function input(pdf) {
  if (typeof pdf === 'string' || Buffer.isBuffer(pdf)) return pdf
  if (pdf instanceof Uint8Array) return Buffer.from(pdf.buffer, pdf.byteOffset, pdf.byteLength)
  if (pdf instanceof ArrayBuffer) return Buffer.from(pdf)
  throw new TypeError('pdf must be a Buffer, a Uint8Array, an ArrayBuffer or a file path')
}

function sync(fn) {
  return (pdf, options) => {
    try {
      return fn(input(pdf), options)
    } catch (err) {
      throw convertError(err)
    }
  }
}

function async(fn) {
  return (pdf, options) => {
    try {
      return fn(input(pdf), options).catch((err) => {
        throw convertError(err)
      })
    } catch (err) {
      return Promise.reject(convertError(err))
    }
  }
}

module.exports = {
  PdfminerError,
  extractText: sync(native.extractTextNative),
  extractTextAsync: async(native.extractTextAsyncNative),
  extractPages: sync(native.extractPagesNative),
  extractPagesAsync: async(native.extractPagesAsyncNative),
  convert: sync(native.convertNative),
  convertAsync: async(native.convertAsyncNative),
  dumpLayout: sync(native.dumpLayoutNative),
  dumpLayoutAsync: async(native.dumpLayoutAsyncNative),
  upstreamVersion: native.upstreamVersion(),
}
