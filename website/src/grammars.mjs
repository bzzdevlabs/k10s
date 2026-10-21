// TextMate grammars of the VS Code extension: one source for editor and docs highlighting.
import ten from '../../editors/vscode/syntaxes/ten.tmLanguage.json' with { type: 'json' };
import k10s from '../../editors/vscode/syntaxes/k10s.tmLanguage.json' with { type: 'json' };

export const langs = [
	{ ...ten, name: 'ten', aliases: ['Ten'] },
	{ ...k10s, name: 'k10s', aliases: ['k10s-manifest'] },
];
