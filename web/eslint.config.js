// @ts-check
// Mosaic management SPA — ESLint flat config (ESLint 9 + typescript-eslint 8).
// Enforces the "absolute typing" pillar of docs/development/agent-guardrails.md:
//   bans `any` (no-explicit-any + no-unsafe-*), @ts-ignore/@ts-nocheck, and non-null `!`.
// strictTypeChecked is NOT semver-stable: pin typescript-eslint and review the
// changelog before upgrading. Type-aware rules silently disable for any file not
// covered by a tsconfig — keep `projectService` pointing at every linted file.
import eslint from '@eslint/js';
import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';

export default tseslint.config(
  // Never lint build output, generated clients, or vendored code.
  { ignores: ['dist/**', 'build/**', 'coverage/**', 'src/api/generated/**', '*.config.js'] },

  eslint.configs.recommended,
  tseslint.configs.strictTypeChecked,
  tseslint.configs.stylisticTypeChecked,

  {
    files: ['**/*.{ts,tsx}'],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: 'module',
      // Type-aware linting — REQUIRED for strictTypeChecked's no-unsafe-* rules.
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: {
      'react-hooks': reactHooks,
      'react-refresh': reactRefresh,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      'react-refresh/only-export-components': ['warn', { allowConstantExport: true }],

      // --- Absolute typing: no untyped escape hatches (made explicit). ---
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/no-non-null-assertion': 'error', // bans the `!` operator
      '@typescript-eslint/no-unsafe-type-assertion': 'error',
      '@typescript-eslint/consistent-type-assertions': [
        'error',
        { assertionStyle: 'as', objectLiteralTypeAssertions: 'never' },
      ],
      '@typescript-eslint/ban-ts-comment': [
        'error',
        {
          'ts-ignore': true, // forbidden — use @ts-expect-error
          'ts-nocheck': true,
          'ts-check': false,
          'ts-expect-error': 'allow-with-description',
          minimumDescriptionLength: 10,
        },
      ],
      // Enforce `import type` so verbatimModuleSyntax in tsconfig stays happy.
      '@typescript-eslint/consistent-type-imports': 'error',
      // No floating/unhandled promises in a React app.
      '@typescript-eslint/no-floating-promises': 'error',
      '@typescript-eslint/no-misused-promises': 'error',
    },
  },

  // Tests may mock — relax the strictest type-flow rules ONLY here, and document why.
  // Note: this does NOT permit weakening assertions; see agent-guardrails.md §B.2.
  {
    files: ['**/*.{test,spec}.{ts,tsx}', '**/__tests__/**/*.{ts,tsx}'],
    rules: {
      '@typescript-eslint/no-non-null-assertion': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
    },
  },
);
