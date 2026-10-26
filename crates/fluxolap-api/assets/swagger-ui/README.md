# Swagger UI (vendored)

`swagger-ui-dist` 5.33.0 from npm (https://www.npmjs.com/package/swagger-ui-dist),
by SmartBear Software, licensed Apache-2.0 (`LICENSE`, `NOTICE`). Third-party
code bundled into `swagger-ui-bundle.js` is listed, with its licenses, in
`swagger-ui-bundle.js.LICENSE.txt`.

The files are embedded into the `fluxolap` binary (`src/docs.rs`) so the
API docs at `/api/v1/docs` work offline. To update, copy the same files
from a newer `swagger-ui-dist` package and change `SWAGGER_UI_VERSION` in
`src/docs.rs`.
