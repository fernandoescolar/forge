/**
 * Visual Studio solution files (.sln). The format is line-oriented and simple, so the
 * grammar only tells its parts apart for highlighting: keywords, section names, project
 * lines (type, name, path, guid) and key = value pairs.
 *
 * Regenerate src/ with: npx tree-sitter-cli@0.25 generate --abi 14
 */
const keyword = word => token(prec(10, word));

module.exports = grammar({
  name: 'sln',

  extras: $ => [/\s/],

  rules: {
    document: $ => repeat($._item),

    _item: $ => choice($.header, $.comment, $.project, $.global, $.assignment),

    header: $ => token(prec(20, /Microsoft Visual Studio Solution File[^\r\n]*/)),

    comment: $ => token(prec(20, /#[^\r\n]*/)),

    project: $ => seq(
      keyword('Project'),
      '(', field('type', $.string), ')',
      '=',
      field('name', $.string), ',', field('path', $.string), ',', field('guid', $.string),
      repeat(choice($.section, $.assignment)),
      keyword('EndProject'),
    ),

    global: $ => seq(keyword('Global'), repeat($.section), keyword('EndGlobal')),

    section: $ => seq(
      choice(keyword('ProjectSection'), keyword('GlobalSection')),
      '(', field('name', $.section_name), ')',
      '=',
      $.timing,
      repeat($.assignment),
      choice(keyword('EndProjectSection'), keyword('EndGlobalSection')),
    ),

    section_name: $ => /[A-Za-z][A-Za-z0-9_]*/,

    timing: $ => choice(keyword('preProject'), keyword('postProject'), keyword('preSolution'), keyword('postSolution')),

    assignment: $ => seq(field('key', $.key), '=', field('value', $.value)),

    key: $ => /[^=\s][^=\r\n]*/,

    value: $ => /[^\s][^\r\n]*/,

    string: $ => /"[^"\r\n]*"/,
  },
});
