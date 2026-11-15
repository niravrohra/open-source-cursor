import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

export interface DocSourceSummary {
  name: string;
  rootUrl: string;
  pagesIndexed: number;
  chunksIndexed: number;
  updatedAt: number;
  usedLlmsTxt: boolean;
}

export interface DocChunkHit {
  sourceName: string;
  url: string;
  title: string;
  sectionHeading: string;
  content: string;
  score: number;
}

export interface McpEnvPrompt {
  key: string;
  label: string;
  required: boolean;
  secret: boolean;
}

export interface McpPreset {
  id: string;
  name: string;
  category: string;
  description: string;
  command: string;
  args: string[];
  envKeys: McpEnvPrompt[];
  docsUrl: string;
}

export function registerDocsAndMcpCommands(
  context: vscode.ExtensionContext,
  client: SidecarClient,
  getWorkspaceRoot: () => string | undefined
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('rursor.addDocsSource', async () => {
      const startUrl = await vscode.window.showInputBox({
        title: 'Rursor @docs: Index Documentation Site',
        prompt: 'Enter documentation root URL or llms.txt URL (e.g., https://orm.drizzle.team/docs/overview)',
        placeHolder: 'https://docs.rs/tokio/latest/tokio/',
        ignoreFocusOut: true,
      });
      if (!startUrl || !startUrl.trim()) {
        return;
      }

      let defaultName = 'docs';
      try {
        const parsed = new URL(startUrl.trim());
        defaultName = parsed.hostname.replace(/^www\.|^docs\./, '').split('.')[0] || 'docs';
      } catch {
        // Ignore URL parse hint errors; sidecar validates strictly
      }

      const name = await vscode.window.showInputBox({
        title: 'Rursor @docs: Source Handle',
        prompt: 'Short @docs handle name for referencing in Rursor Agent',
        value: defaultName,
        ignoreFocusOut: true,
      });
      if (!name || !name.trim()) {
        return;
      }

      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: `Rursor: Indexing @docs "${name.trim()}"...`,
        },
        async () => {
          const res = await client.request<{ source: DocSourceSummary }>('index_docs_source', {
            name: name.trim(),
            startUrl: startUrl.trim(),
            maxPages: 25,
          });
          const s = res.source;
          void vscode.window.showInformationMessage(
            `Indexed @docs/${s.name}: ${s.pagesIndexed} page(s), ${s.chunksIndexed} chunks${s.usedLlmsTxt ? ' (via llms.txt fast path)' : ''}.`
          );
        }
      );
    }),

    vscode.commands.registerCommand('rursor.manageDocsSources', async () => {
      const res = await client.request<{ sources: DocSourceSummary[] }>('list_docs_sources', {});
      const sources = res.sources ?? [];

      const items: Array<vscode.QuickPickItem & { action: string; source?: DocSourceSummary }> = [
        {
          label: '$(add) Index New @docs Site...',
          description: 'Crawl HTML docs or llms.txt into persistent hybrid FTS5 + vector store',
          action: 'add',
        },
        {
          label: '$(search) Search Indexed @docs...',
          description: `${sources.length} source(s) indexed`,
          action: 'search',
        },
      ];

      for (const src of sources) {
        items.push({
          label: `$(book) @docs/${src.name}`,
          description: `${src.pagesIndexed} pages · ${src.chunksIndexed} chunks${src.usedLlmsTxt ? ' · llms.txt' : ''}`,
          detail: src.rootUrl,
          action: 'manage_one',
          source: src,
        });
      }

      const picked = await vscode.window.showQuickPick(items, {
        title: 'Rursor @docs Persistent Documentation Index',
        placeHolder: 'Add, search, re-index, or remove documentation sources',
      });
      if (!picked) {
        return;
      }

      if (picked.action === 'add') {
        await vscode.commands.executeCommand('rursor.addDocsSource');
        return;
      }

      if (picked.action === 'search') {
        const q = await vscode.window.showInputBox({
          title: 'Search @docs Knowledge Base',
          placeHolder: 'e.g., async stream cancellation or migration push',
        });
        if (!q || !q.trim()) {
          return;
        }
        const hitRes = await client.request<{ hits: DocChunkHit[] }>('search_docs', {
          query: q.trim(),
          limit: 8,
        });
        const hits = hitRes.hits ?? [];
        if (hits.length === 0) {
          void vscode.window.showInformationMessage(`No @docs matches found for "${q.trim()}".`);
          return;
        }
        const docContent = hits
          .map(
            (h, idx) =>
              `## ${idx + 1}. [${h.sourceName}] ${h.title} — ${h.sectionHeading}\nSource: ${h.url}\n\n${h.content}\n`
          )
          .join('\n---\n\n');
        const doc = await vscode.workspace.openTextDocument({
          language: 'markdown',
          content: `# Rursor @docs Search: ${q.trim()}\n\n${docContent}`,
        });
        await vscode.window.showTextDocument(doc, { preview: true });
        return;
      }

      if (picked.action === 'manage_one' && picked.source) {
        const src = picked.source;
        const sub = await vscode.window.showQuickPick(
          [
            { label: '$(sync) Re-crawl Source', id: 'recrawl' },
            { label: '$(link-external) Open Docs URL in Browser', id: 'open' },
            { label: '$(trash) Remove @docs Source', id: 'remove' },
          ],
          { title: `@docs/${src.name}` }
        );
        if (!sub) {
          return;
        }
        if (sub.id === 'recrawl') {
          await vscode.window.withProgress(
            {
              location: vscode.ProgressLocation.Notification,
              title: `Re-crawling @docs/${src.name}...`,
            },
            async () => {
              const r = await client.request<{ source: DocSourceSummary }>('index_docs_source', {
                name: src.name,
                startUrl: src.rootUrl,
                maxPages: 25,
              });
              void vscode.window.showInformationMessage(
                `Updated @docs/${r.source.name} (${r.source.chunksIndexed} chunks).`
              );
            }
          );
        } else if (sub.id === 'open') {
          await vscode.env.openExternal(vscode.Uri.parse(src.rootUrl));
        } else if (sub.id === 'remove') {
          await client.request('remove_docs_source', { name: src.name });
          void vscode.window.showInformationMessage(`Removed @docs/${src.name}.`);
        }
      }
    }),

    vscode.commands.registerCommand('rursor.installMcpPreset', async () => {
      const catRes = await client.request<{ presets: McpPreset[] }>('list_mcp_catalog', {});
      const presets = catRes.presets ?? [];
      if (presets.length === 0) {
        void vscode.window.showWarningMessage('MCP catalog is empty.');
        return;
      }

      const picked = await vscode.window.showQuickPick(
        presets.map((p) => ({
          label: `$(plug) ${p.name}`,
          description: `[${p.category}] ${p.command} ${p.args.join(' ')}`,
          detail: p.description,
          preset: p,
        })),
        {
          title: 'Rursor 1-Click MCP Server App Store',
          placeHolder: 'Select an MCP integration to install into .rursor/mcp.json',
        }
      );
      if (!picked) {
        return;
      }

      const envValues: Record<string, string> = {};
      for (const promptSpec of picked.preset.envKeys) {
        const val = await vscode.window.showInputBox({
          title: `${picked.preset.name} — ${promptSpec.key}`,
          prompt: promptSpec.label,
          password: promptSpec.secret,
          ignoreFocusOut: true,
        });
        if (promptSpec.required && (!val || !val.trim())) {
          void vscode.window.showWarningMessage(`Cancelled ${picked.preset.name} setup (${promptSpec.key} required).`);
          return;
        }
        if (val && val.trim()) {
          envValues[promptSpec.key] = val.trim();
        }
      }

      const wsRoot = getWorkspaceRoot();
      const targetScope = wsRoot
        ? await vscode.window.showQuickPick(
            [
              {
                label: '$(folder) Workspace (.rursor/mcp.json)',
                description: path.join(wsRoot, '.rursor', 'mcp.json'),
                scope: 'workspace',
              },
              {
                label: '$(globe) Global (~/.rursor/mcp.json)',
                description: 'Available in every Rursor project',
                scope: 'global',
              },
            ],
            { title: `Where should ${picked.preset.name} be installed?` }
          )
        : { scope: 'global' };

      if (!targetScope) {
        return;
      }

      const installRes = await client.request<{ configPath: string }>('install_mcp_preset', {
        presetId: picked.preset.id,
        scope: targetScope.scope,
        workspaceRoot: wsRoot,
        envValues,
      });

      try {
        await client.request('reload_mcp', { workspaceRoot: wsRoot });
      } catch {
        // Server will reload when npx/uvx starts next
      }

      void vscode.window.showInformationMessage(
        `Installed MCP server "${picked.preset.name}" in ${installRes.configPath}.`
      );
    })
  );
}
