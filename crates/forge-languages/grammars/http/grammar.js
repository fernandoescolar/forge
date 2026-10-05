/**
 * .http / .rest request files (Visual Studio, VS Code REST Client). Line oriented: the
 * grammar tells the kinds of lines apart for highlighting and for the run button on each
 * request line; forge-http reads the requests themselves.
 *
 * Regenerate src/ with: npx tree-sitter-cli@0.25 generate --abi 14
 */
const METHOD = /GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|TRACE|CONNECT/;

module.exports = grammar({
  name: 'http',

  extras: $ => [/[ \t]/],

  rules: {
    document: $ => seq(repeat(choice(seq($._line, $._newline), $._newline)), optional($._line)),

    _newline: $ => /\r?\n/,

    _line: $ => choice($.separator, $.comment, $.variable_declaration, $.request_line, $.header, $.body_line),

    separator: $ => token(prec(6, /###[^\r\n]*/)),

    comment: $ => token(prec(5, /(#|\/\/)[^\r\n]*/)),

    variable_declaration: $ => seq(field('name', $.variable_name), '=', optional(field('value', $._text))),

    variable_name: $ => token(prec(4, /@[A-Za-z_][A-Za-z0-9_.-]*/)),

    request_line: $ => seq(
      field('method', $.method),
      field('url', $.url),
      optional(field('version', $.version)),
    ),

    method: $ => token(prec(3, METHOD)),

    url: $ => repeat1(choice($.variable, /[^\s{]+/, '{')),

    version: $ => token(prec(3, /HTTP\/[0-9.]+/)),

    header: $ => seq(field('name', $.header_name), optional(field('value', $._text))),

    header_name: $ => token(prec(2, /[A-Za-z][A-Za-z0-9_-]*:/)),

    body_line: $ => $._text,

    _text: $ => repeat1(choice($.variable, $.text)),

    text: $ => token(prec(-1, /[^{\r\n]+|\{/)),

    variable: $ => token(prec(7, /\{\{[^}\r\n]*\}\}/)),
  },
});
