[out:json][timeout:90][date:"2026-11-22T12:24:51Z"][bbox:46.343,14.133,46.378,14.188];
(
  way["highway"];
  way["building"];
  way["landuse"];
  way["natural"="wood"];
  way["railway"];
  nwr["amenity"~"^(school|kindergarten|fire_station|police|hospital|clinic|doctors|pharmacy|townhall|post_office)$"];
  nwr["railway"~"^(station|halt)$"];
  relation["building"]["type"="multipolygon"];
  relation["landuse"]["type"="multipolygon"];
  relation["natural"="wood"]["type"="multipolygon"];
);
out body geom;
