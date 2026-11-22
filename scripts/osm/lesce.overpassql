[out:json][timeout:90][bbox:46.343,14.133,46.378,14.188];
(
  way["highway"];
  way["building"];
  way["landuse"];
  way["railway"];
  nwr["amenity"~"^(school|kindergarten|fire_station|police|hospital|clinic|doctors|pharmacy|townhall|post_office)$"];
  nwr["railway"~"^(station|halt)$"];
  relation["building"]["type"="multipolygon"];
  relation["landuse"]["type"="multipolygon"];
);
out body geom;
