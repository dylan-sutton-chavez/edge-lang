import composition from './composition.ts';
import { listen } from './listen.ts';

// the JS host's own bundle while another author bundles a composition of their own
listen(composition);
