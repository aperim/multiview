// A small generic TanStack-Table wrapper for the read-only resource views.
//
// Keeps the Sources/Outputs/Overlays views DRY: pass the typed rows + columns
// and it renders an accessible table with loading/empty states. It is read-only
// by design — these resources are stubbed until their write APIs ship.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
} from '@tanstack/react-table';
import type { ColumnDef } from '@tanstack/react-table';

import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';

/** Props for {@link ResourceTable}. */
export interface ResourceTableProps<T> {
  /** The rows to render. */
  readonly rows: readonly T[];
  /** Column definitions (TanStack Table). */
  readonly columns: ColumnDef<T>[];
  /** Accessible table caption. */
  readonly caption: string;
  /** Message shown when there are no rows. */
  readonly emptyMessage: JSX.Element;
}

/** A read-only, accessible resource list table. */
export function ResourceTable<T>({
  rows,
  columns,
  caption,
  emptyMessage,
}: ResourceTableProps<T>): JSX.Element {
  // eslint-disable-next-line react-hooks/incompatible-library -- TanStack Table instance is a leaf; see LayoutsPage note.
  const table = useReactTable<T>({
    data: [...rows],
    columns,
    getCoreRowModel: getCoreRowModel(),
  });

  if (rows.length === 0) {
    return <p className="text-sm text-muted-foreground">{emptyMessage}</p>;
  }

  return (
    <Table>
      <TableCaption>{caption}</TableCaption>
      <TableHeader>
        {table.getHeaderGroups().map((group) => (
          <TableRow key={group.id}>
            {group.headers.map((header) => (
              <TableHead key={header.id}>
                {header.isPlaceholder
                  ? null
                  : flexRender(
                      header.column.columnDef.header,
                      header.getContext(),
                    )}
              </TableHead>
            ))}
          </TableRow>
        ))}
      </TableHeader>
      <TableBody>
        {table.getRowModel().rows.map((row) => (
          <TableRow key={row.id}>
            {row.getVisibleCells().map((cell) => (
              <TableCell key={cell.id}>
                {flexRender(cell.column.columnDef.cell, cell.getContext())}
              </TableCell>
            ))}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

/** A standard "API not wired yet" banner for the stubbed resource views. */
export function StubNotice(): JSX.Element {
  return (
    <p
      role="note"
      className="mb-4 rounded-md border border-dashed bg-muted/40 px-3 py-2 text-sm text-muted-foreground"
    >
      <Trans>
        Sample data shown. This resource is read-only until its management API
        ships; the view is already typed to swap to live data with no change.
      </Trans>
    </p>
  );
}
