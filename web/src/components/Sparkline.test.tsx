// Sparkline: a dependency-free inline-SVG trend line. It must render an
// accessible <svg role="img"> with the caller's aria-label and a <polyline>
// whose points are the values normalised into the [0, max] band (default
// max = 1). The y-axis is inverted (SVG origin is top-left) so a higher value
// sits higher on the chart.
import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';

import { Sparkline } from './Sparkline';

describe('Sparkline', () => {
  it('renders an accessible svg with the given aria-label', () => {
    render(<Sparkline values={[0, 0.5, 1]} ariaLabel="CPU trend" />);
    const svg = screen.getByRole('img', { name: 'CPU trend' });
    expect(svg.tagName.toLowerCase()).toBe('svg');
  });

  it('renders a polyline with one point per value', () => {
    const { container } = render(
      <Sparkline values={[0, 0.5, 1]} ariaLabel="trend" width={100} height={20} />,
    );
    const polyline = container.querySelector('polyline');
    expect(polyline).not.toBeNull();
    const points = polyline?.getAttribute('points')?.trim().split(/\s+/) ?? [];
    expect(points).toHaveLength(3);
  });

  it('normalises values into [0, max] and inverts the y-axis', () => {
    // With max=1, height=10: value 1 maps to y=0 (top), value 0 to y=10 (bottom).
    const { container } = render(
      <Sparkline values={[0, 1]} ariaLabel="trend" width={10} height={10} max={1} />,
    );
    const points = container
      .querySelector('polyline')
      ?.getAttribute('points')
      ?.trim()
      .split(/\s+/) ?? [];
    // First point: x=0, value 0 -> y=10 (bottom). Last point: x=10, value 1 -> y=0 (top).
    const first = points[0];
    const last = points[points.length - 1];
    expect(first).toBe('0,10');
    expect(last).toBe('10,0');
  });

  it('clamps out-of-range values to the [0, max] band', () => {
    // A value above max and below 0 must not escape the drawing box.
    const { container } = render(
      <Sparkline
        values={[-5, 5]}
        ariaLabel="trend"
        width={10}
        height={10}
        max={1}
      />,
    );
    const points = container
      .querySelector('polyline')
      ?.getAttribute('points')
      ?.trim()
      .split(/\s+/) ?? [];
    // -5 clamps to 0 -> y=10 (bottom); 5 clamps to max(1) -> y=0 (top).
    expect(points[0]).toBe('0,10');
    expect(points[points.length - 1]).toBe('10,0');
  });

  it('renders an empty (no polyline) chart when there are no values', () => {
    const { container } = render(<Sparkline values={[]} ariaLabel="empty trend" />);
    expect(screen.getByRole('img', { name: 'empty trend' })).toBeInTheDocument();
    expect(container.querySelector('polyline')).toBeNull();
  });
});
