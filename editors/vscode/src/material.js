// @ts-check
'use strict';

const vscode = require('vscode');
const path = require('path');

const MATERIAL = 'PKief.material-icon-theme';
const SECTION = 'material-icon-theme';
const KEY = 'folders.associations';
/** Folder names shown with our icons. */
const FOLDERS = { k10s: 'folder-k10s', '.k10s': 'folder-k10s', ten: 'folder-ten' };
const PROMPTED = 'materialIconTheme.prompted';

/**
 * Material Icon Theme folder icons for Ten and k10s. File icons need nothing (the theme falls back to the icons
 * the languages contribute), but only the active icon theme draws folders: Material Icon Theme accepts custom SVG
 * folder icons through `material-icon-theme.folders.associations` in the user settings, as paths relative to its
 * `icons` directory, provided they are in the same extensions directory. The paths contain this extension's
 * version, so they are refreshed after an update.
 * @param {vscode.ExtensionContext} context
 */
function setup(context) {
    const prefix = `${context.extension.id.toLowerCase()}-`;

    /** @param {unknown} v */
    const ours = (v) => typeof v === 'string' && v.split('/').some((part) => part.startsWith(prefix));

    /** Our association values, or the reason Material Icon Theme cannot use them. */
    const wanted = () => {
        const material = vscode.extensions.getExtension(MATERIAL);
        if (!material) {
            return 'Material Icon Theme is not installed.';
        }
        if (path.dirname(material.extensionPath) !== path.dirname(context.extensionPath)) {
            return 'Material Icon Theme only loads icons from its own extensions directory, and this extension is elsewhere.';
        }
        const base = path.join(material.extensionPath, 'icons');
        /** @type {Record<string, string>} */
        const out = {};
        for (const [folder, icon] of Object.entries(FOLDERS)) {
            const rel = path.relative(base, path.join(context.extensionPath, 'icons', icon)).split(path.sep).join('/');
            // The theme prefixes `folder-` to names that lack it; a leading `folder-…/..` makes that a no-op.
            out[folder] = `${icon}/../${rel}`;
        }
        return out;
    };

    /** @returns {Record<string, unknown>} */
    const current = () => ({ ...vscode.workspace.getConfiguration(SECTION).inspect(KEY)?.globalValue });

    /** @param {Record<string, unknown>} value */
    const write = (value) =>
        vscode.workspace
            .getConfiguration(SECTION)
            .update(KEY, Object.keys(value).length ? value : undefined, vscode.ConfigurationTarget.Global);

    /** Adds our associations, keeping the user's own (an existing association for one of our names wins). */
    const add = async (/** @type {boolean} */ interactive) => {
        const w = wanted();
        if (typeof w === 'string') {
            if (interactive) {
                vscode.window.showWarningMessage(w);
            }
            return;
        }
        const cur = current();
        const next = Object.fromEntries(Object.entries(cur).filter(([, v]) => !ours(v)));
        for (const [folder, value] of Object.entries(w)) {
            if (!(folder in next)) {
                next[folder] = value;
            }
        }
        if (JSON.stringify(next) !== JSON.stringify(cur)) {
            await write(next);
        }
    };

    const remove = async () => {
        const cur = current();
        const next = Object.fromEntries(Object.entries(cur).filter(([, v]) => !ours(v)));
        if (Object.keys(next).length !== Object.keys(cur).length) {
            await write(next);
        }
    };

    context.subscriptions.push(
        vscode.commands.registerCommand('k10s.materialIcons.add', () => add(true)),
        vscode.commands.registerCommand('k10s.materialIcons.remove', remove),
    );

    const installed = Object.values(current()).some(ours);
    if (installed) {
        add(false); // paths of a previous version of this extension
    } else if (
        !context.globalState.get(PROMPTED) &&
        vscode.workspace.getConfiguration('workbench').get('iconTheme') === SECTION &&
        typeof wanted() !== 'string'
    ) {
        context.globalState.update(PROMPTED, true);
        const yes = 'Add folder icons';
        vscode.window
            .showInformationMessage(
                'Show Ten and k10s folder icons (folders named k10s, .k10s, ten) in Material Icon Theme? ' +
                    'This adds entries to material-icon-theme.folders.associations in your user settings.',
                yes,
                'No thanks',
            )
            .then((choice) => choice === yes && add(true));
    }
}

module.exports = { setup };
