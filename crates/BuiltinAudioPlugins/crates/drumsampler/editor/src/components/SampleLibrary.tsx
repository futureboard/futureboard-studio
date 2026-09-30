import { useMemo, useState } from 'react'
import type { SampleFile } from '../bridge'

/// The plugin's Samples folder. Click a file to load it onto the selected pad;
/// Browse… imports a file from anywhere into the folder and loads it. Files
/// dropped here are only added to the folder (`data-drop="library"`).
export function SampleLibrary({
  files,
  current,
  padLabel,
  canLoad,
  onLoad,
  onBrowse,
  onRefresh,
  dropActive,
}: {
  files: SampleFile[] | null
  current: string | null
  padLabel: string
  canLoad: boolean
  onLoad: (fileName: string) => void
  onBrowse: () => void
  onRefresh: () => void
  dropActive: boolean
}) {
  const [query, setQuery] = useState('')
  const shown = useMemo(() => {
    const needle = query.trim().toLowerCase()
    return (files ?? [])
      .filter((file) => !needle || file.fileName.toLowerCase().includes(needle))
      .sort((a, b) => a.fileName.localeCompare(b.fileName))
  }, [files, query])

  return (
    <section className={dropActive ? 'library is-drop-target' : 'library'} data-drop="library">
      <header>
        <h2 className="cap">Samples</h2>
        <span className="library-target">→ {padLabel}</span>
        <button type="button" className="ghost" onClick={onRefresh} title="Re-read the Samples folder">
          Refresh
        </button>
        <button type="button" className="primary" onClick={onBrowse} disabled={!canLoad}>
          Browse…
        </button>
      </header>
      <input
        type="search"
        className="library-search"
        placeholder="Search samples"
        aria-label="Search samples"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
      />
      <div className="library-list" role="listbox" aria-label="Samples folder">
        {files === null ? (
          <p className="library-note">Reading the Samples folder…</p>
        ) : shown.length === 0 ? (
          <p className="library-note">
            {files.length === 0
              ? 'The Samples folder is empty — drop audio files here or use Browse…'
              : `No sample matches “${query}”.`}
          </p>
        ) : (
          shown.map((file) => (
            <button
              key={file.fileName}
              type="button"
              role="option"
              aria-selected={file.fileName === current}
              className={file.fileName === current ? 'is-current' : ''}
              disabled={!canLoad}
              onClick={() => onLoad(file.fileName)}
              title={`Load ${file.fileName} onto ${padLabel}`}
            >
              <span className="library-name">{file.fileName}</span>
              <span className="library-size num">{Math.max(1, Math.round(file.sizeBytes / 1024))} KB</span>
            </button>
          ))
        )}
      </div>
    </section>
  )
}
